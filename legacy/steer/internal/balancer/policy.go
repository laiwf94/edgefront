package balancer

import (
	"sync"

	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/endpoint"
)

// RoundRobin is nginx's smooth weighted round robin. Each pick adds every
// candidate's weight to its running score, takes the highest score and
// subtracts the total weight from it. Over any run of picks with a fixed
// candidate set, each endpoint is chosen in proportion to its weight and the
// choices are spread out (5,1,1 gives a a b a c a a, not a a a a a b c).
type RoundRobin struct {
	eps     []*endpoint.Endpoint
	mu      sync.Mutex
	current []int64
}

// NewRoundRobin returns a smooth weighted round robin over eps.
func NewRoundRobin(eps []*endpoint.Endpoint) *RoundRobin {
	return &RoundRobin{eps: eps, current: make([]int64, len(eps))}
}

// Pick implements Policy.
func (r *RoundRobin) Pick(cands []int, skip func(int) bool, _ uint64, _ bool) int {
	r.mu.Lock()
	defer r.mu.Unlock()
	best := -1
	var total int64
	for _, i := range cands {
		if skip(i) {
			continue
		}
		w := int64(r.eps[i].Weight)
		r.current[i] += w
		total += w
		if best < 0 || r.current[i] > r.current[best] {
			best = i
		}
	}
	if best >= 0 {
		r.current[best] -= total
	}
	return best
}

// LeastLoaded is power of two choices: pick two candidates at random and
// keep the one with the lower cost, in-flight requests x peak-EWMA latency
// (Mitzenmacher's "power of two choices"; Finagle's and Linkerd's P2C with
// peak EWMA). It avoids the herding of "always pick the least loaded" when
// many proxies share the same stale view, and costs O(1) per pick.
type LeastLoaded struct {
	eps  []*endpoint.Endpoint
	rand func(int) int
	clk  clock.Clock
}

// NewLeastLoaded returns a P2C policy over eps.
func NewLeastLoaded(eps []*endpoint.Endpoint, rand func(int) int, clk clock.Clock) *LeastLoaded {
	return &LeastLoaded{eps: eps, rand: rand, clk: clk}
}

// Pick implements Policy.
func (l *LeastLoaded) Pick(cands []int, skip func(int) bool, _ uint64, _ bool) int {
	pool := cands
	for k, i := range cands {
		if skip(i) {
			// Rare (retries only): copy the unskipped candidates.
			pool = make([]int, 0, len(cands)-1)
			pool = append(pool, cands[:k]...)
			for _, j := range cands[k+1:] {
				if !skip(j) {
					pool = append(pool, j)
				}
			}
			break
		}
	}
	switch len(pool) {
	case 0:
		return -1
	case 1:
		return pool[0]
	}
	a := l.rand(len(pool))
	b := l.rand(len(pool) - 1)
	if b >= a {
		b++
	}
	ia, ib := pool[a], pool[b]
	now := l.clk.Now()
	if l.eps[ib].Cost(now) < l.eps[ia].Cost(now) {
		return ib
	}
	return ia
}
