// Package balancer picks an upstream endpoint for each request attempt.
//
// Picking happens in two steps, following Envoy's load balancer: first a
// priority level is chosen from the share of healthy hosts in each level
// (with an overprovisioning factor and a panic threshold), then a policy picks
// a host inside that level. Policies are smooth weighted round robin,
// power-of-two-choices on in-flight x EWMA latency, and Maglev consistent
// hashing.
package balancer

import (
	"fmt"
	"math/rand/v2"
	"slices"

	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
)

// OverprovisioningFactor is Envoy's default 1.4, in percent: a level whose
// healthy share is at least 1/1.4 (about 72%) still takes all of its load.
const OverprovisioningFactor = 140

// Policy picks one endpoint among candidates. cands are indices into the
// endpoint slice the policy was built with; skip marks indices that must not
// be returned (already tried by this request). Pick returns -1 when every
// candidate is skipped.
type Policy interface {
	Pick(cands []int, skip func(int) bool, key uint64, hasKey bool) int
}

// PriorityLoad computes the percentage of traffic each priority level
// receives, following Envoy's priority load math:
//
//	health[p] = min(100, 1.4 x 100 x healthy[p] / total[p])
//	normalized = min(100, sum(health))
//	load[p]   = min(remaining, health[p] x 100 / normalized), in priority order
//
// Rounding leftovers go to the first level with any health. panic reports that
// normalized health is below panicThreshold (percent; 0 disables panic), in
// which case health should be ignored. load is nil when no level has a healthy
// host and the cluster is not in panic.
func PriorityLoad(healthy, total []int, panicThreshold int) (load []int, panic bool) {
	health := make([]int, len(total))
	sum := 0
	for p := range total {
		if total[p] > 0 {
			health[p] = min(100, OverprovisioningFactor*healthy[p]/total[p])
		}
		sum += health[p]
	}
	normalized := min(100, sum)
	if normalized < panicThreshold {
		return nil, true
	}
	if normalized == 0 {
		return nil, false
	}
	load = make([]int, len(total))
	remaining := 100
	first := -1
	for p := range health {
		load[p] = min(remaining, health[p]*100/normalized)
		remaining -= load[p]
		if first < 0 && health[p] > 0 {
			first = p
		}
	}
	load[first] += remaining
	return load, false
}

// Options configure an LB.
type Options struct {
	Policy         config.Policy
	PanicThreshold int // percent
	Clock          clock.Clock
	// Rand returns a uniform integer in [0, n). Defaults to math/rand/v2.
	Rand func(n int) int
	// MaglevTableSize must be prime; defaults to DefaultMaglevTableSize.
	MaglevTableSize int
}

// LB balances over a fixed set of endpoints. It is safe for concurrent use.
type LB struct {
	eps            []*endpoint.Endpoint
	all            []int
	levels         int
	policy         Policy
	hashing        bool
	panicThreshold int
	clk            clock.Clock
	rand           func(int) int
}

// New builds a load balancer. eps must not be empty.
func New(eps []*endpoint.Endpoint, opts Options) (*LB, error) {
	if len(eps) == 0 {
		return nil, fmt.Errorf("balancer: no endpoints")
	}
	lb := &LB{eps: eps, panicThreshold: opts.PanicThreshold, clk: opts.Clock, rand: opts.Rand}
	if lb.clk == nil {
		lb.clk = clock.Real{}
	}
	if lb.rand == nil {
		lb.rand = rand.IntN
	}
	for i, ep := range eps {
		if ep.Weight < 1 || ep.Priority < 0 || ep.Priority > config.MaxPriority {
			return nil, fmt.Errorf("balancer: endpoint %s has weight %d priority %d", ep.Address, ep.Weight, ep.Priority)
		}
		lb.all = append(lb.all, i)
		lb.levels = max(lb.levels, ep.Priority+1)
	}
	switch opts.Policy {
	case config.RoundRobin, "":
		lb.policy = NewRoundRobin(eps)
	case config.LeastLoaded:
		lb.policy = NewLeastLoaded(eps, lb.rand, lb.clk)
	case config.Maglev:
		size := opts.MaglevTableSize
		if size == 0 {
			size = DefaultMaglevTableSize
		}
		m, err := NewMaglev(eps, size, lb.rand)
		if err != nil {
			return nil, err
		}
		lb.policy, lb.hashing = m, true
	default:
		return nil, fmt.Errorf("balancer: unknown policy %q", opts.Policy)
	}
	return lb, nil
}

// Endpoints returns the endpoints in configuration order.
func (lb *LB) Endpoints() []*endpoint.Endpoint { return lb.eps }

// Pick returns the index of the endpoint for the next attempt, never one in
// tried, or -1 when no available endpoint is left. key is the request's hash
// key when hasKey is set (Maglev only).
func (lb *LB) Pick(key uint64, hasKey bool, tried []int) int {
	now := lb.clk.Now()
	var healthyArr, totalArr [config.MaxPriority + 1]int
	var availArr [config.MaxPriority + 1][]int
	healthy, total, avail := healthyArr[:lb.levels], totalArr[:lb.levels], availArr[:lb.levels]
	for i, ep := range lb.eps {
		p := ep.Priority
		total[p]++
		if ep.Available(now) {
			healthy[p]++
			avail[p] = append(avail[p], i)
		}
	}
	skip := func(i int) bool { return slices.Contains(tried, i) }
	load, panic := PriorityLoad(healthy, total, lb.panicThreshold)
	if panic {
		return lb.policy.Pick(lb.all, skip, key, hasKey)
	}
	if load == nil {
		return -1
	}
	// Hashing policies choose the level from the key too, so a key keeps its
	// endpoint for as long as the health picture does not change.
	var roll int
	if lb.hashing && hasKey {
		roll = int((key >> 32) % 100)
	} else {
		roll = lb.rand(100)
	}
	chosen := 0
	for p, l := range load {
		if roll < l {
			chosen = p
			break
		}
		roll -= l
	}
	if i := lb.policy.Pick(avail[chosen], skip, key, hasKey); i >= 0 {
		return i
	}
	// Every available host in the chosen level was already tried; fall back
	// to the other levels in priority order.
	for p := range avail {
		if p == chosen {
			continue
		}
		if i := lb.policy.Pick(avail[p], skip, key, hasKey); i >= 0 {
			return i
		}
	}
	return -1
}

// State describes the current priority picture.
type State struct {
	Healthy      []int `json:"healthy_per_priority"`
	Total        []int `json:"total_per_priority"`
	PriorityLoad []int `json:"priority_load_percent"`
	Panic        bool  `json:"panic"`
}

// State computes the priority picture now.
func (lb *LB) State() State {
	now := lb.clk.Now()
	st := State{Healthy: make([]int, lb.levels), Total: make([]int, lb.levels)}
	for _, ep := range lb.eps {
		st.Total[ep.Priority]++
		if ep.Available(now) {
			st.Healthy[ep.Priority]++
		}
	}
	st.PriorityLoad, st.Panic = PriorityLoad(st.Healthy, st.Total, lb.panicThreshold)
	if st.PriorityLoad == nil {
		st.PriorityLoad = make([]int, lb.levels)
	}
	return st
}
