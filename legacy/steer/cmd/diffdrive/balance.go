package main

// The picks query runs one balancing scenario through steer's balancer.LB:
//
//	{"op":"picks","policy":"maglev","panic_threshold":50,"seed":7,"table_size":251,
//	 "endpoints":[{"address":"10.0.0.1:80","weight":1,"priority":0}],
//	 "script":"r 12345 2\nh 0 0\ns"}
//	{"ok":true,"lines":["3,1","ok","state h=0 t=1 l=0 panic=false"]}
//
// The balancer draws from a SplitMix64 seeded with seed and reads a fake
// clock that starts at Unix time 1,700,000,000. The script has one
// operation per line and the answer one line per operation:
//
//	r KEY N        a request: up to N attempts, each a Pick with the request's
//	               key (a decimal uint64, or - for none) and the endpoints its
//	               earlier attempts picked; it stops at the first -1. The
//	               line lists the picks: 3,1 or 4,-1 or -1.
//	s              the priority state: state h=H,.. t=T,.. l=L,.. panic=BOOL
//	h I 0|1        SetHealthy on endpoint I
//	e I BASE MAX   Eject endpoint I now for BASE x times ejected, capped at
//	               MAX (nanoseconds; BASE must be positive)
//	c I            ClearEjection on endpoint I
//	a I / x I      Acquire / Release one request on endpoint I
//	o I NS         endpoint I observes a latency of NS nanoseconds now
//	t NS           the clock advances NS nanoseconds
//
// Every line but r and s answers ok. A malformed line is a bug in the
// caller and stops the driver with an error.

import (
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/laiwf94/steer/internal/balancer"
	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
)

// pickEpoch is where the fake clock of every scenario starts.
var pickEpoch = time.Unix(1_700_000_000, 0)

type endpointSpec struct {
	Address  string `json:"address"`
	Weight   int    `json:"weight"`
	Priority int    `json:"priority"`
}

func picks(q *query) (answer, error) {
	eps := make([]*endpoint.Endpoint, len(q.Endpoints))
	for i, e := range q.Endpoints {
		eps[i] = endpoint.New(e.Address, e.Weight, e.Priority)
	}
	clk := clock.NewFake(pickEpoch)
	lb, err := balancer.New(eps, balancer.Options{
		Policy:          config.Policy(q.Policy),
		PanicThreshold:  q.PanicThreshold,
		Clock:           clk,
		Rand:            newSplitMix(q.Seed).IntN,
		MaglevTableSize: q.TableSize,
	})
	if err != nil {
		return answer{Class: "balancer", Error: err.Error()}, nil
	}
	ops := strings.Split(q.Script, "\n")
	lines := make([]string, 0, len(ops))
	var tried []int
	var b strings.Builder
	for n, op := range ops {
		f := strings.Fields(op)
		if len(f) == 0 {
			return answer{}, fmt.Errorf("op %d: empty", n+1)
		}
		ints := numbers(f[1:])
		ep := len(ints) > 0 && ints[0] >= 0 && ints[0] < int64(len(eps))
		bad := func() (answer, error) { return answer{}, fmt.Errorf("op %d %q: malformed", n+1, op) }
		line := "ok"
		switch {
		case f[0] == "r" && len(f) == 3:
			key, hasKey := uint64(0), f[1] != "-"
			if hasKey {
				if key, err = strconv.ParseUint(f[1], 10, 64); err != nil {
					return bad()
				}
			}
			attempts, err := strconv.Atoi(f[2])
			if err != nil || attempts < 1 {
				return bad()
			}
			tried = tried[:0]
			b.Reset()
			for k := range attempts {
				i := lb.Pick(key, hasKey, tried)
				if k > 0 {
					b.WriteByte(',')
				}
				b.WriteString(strconv.Itoa(i))
				if i < 0 {
					break
				}
				tried = append(tried, i)
			}
			line = b.String()
		case f[0] == "s" && len(f) == 1:
			st := lb.State()
			line = fmt.Sprintf("state h=%s t=%s l=%s panic=%t", joinInts(st.Healthy), joinInts(st.Total), joinInts(st.PriorityLoad), st.Panic)
		case f[0] == "h" && len(f) == 3 && ep && ints[1] >= 0 && ints[1] <= 1:
			eps[ints[0]].SetHealthy(ints[1] == 1)
		case f[0] == "e" && len(f) == 4 && ep && ints[1] > 0 && ints[2] >= 0:
			eps[ints[0]].Eject(clk.Now(), time.Duration(ints[1]), time.Duration(ints[2]), "difftest")
		case f[0] == "c" && len(f) == 2 && ep:
			eps[ints[0]].ClearEjection()
		case f[0] == "a" && len(f) == 2 && ep:
			eps[ints[0]].Acquire()
		case f[0] == "x" && len(f) == 2 && ep:
			eps[ints[0]].Release()
		case f[0] == "o" && len(f) == 3 && ep && ints[1] >= 0:
			eps[ints[0]].ObserveLatency(time.Duration(ints[1]), clk.Now())
		case f[0] == "t" && len(f) == 2 && ints[0] >= 0:
			clk.Advance(time.Duration(ints[0]))
		default:
			return bad()
		}
		lines = append(lines, line)
	}
	return answer{OK: true, Lines: lines}, nil
}

// numbers parses the arguments of an op as int64s: endpoint indices,
// nanoseconds and flags. One that is not a decimal int64 (a key above
// MaxInt64, or the - of a keyless request) parses as -1, which no check
// accepts; r reads its key again itself.
func numbers(args []string) []int64 {
	out := make([]int64, len(args))
	for i, a := range args {
		v, err := strconv.ParseInt(a, 10, 64)
		if err != nil {
			v = -1
		}
		out[i] = v
	}
	return out
}

func joinInts(xs []int) string {
	s := make([]string, len(xs))
	for i, x := range xs {
		s[i] = strconv.Itoa(x)
	}
	return strings.Join(s, ",")
}
