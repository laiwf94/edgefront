package main

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/balancer"
	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/endpoint"
)

// pickVectors is shared with the Rust port, which replays every case with
// its own round robin, power of two choices and SplitMix64 and must make the
// same picks. Run with STEER_BLESS=1 to rewrite it from steer.
const pickVectors = "../../../../crates/edgefront-proxy/tests/fixtures/steer_picks.txt"

// Reference outputs of SplitMix64 for the fixture: the Uint64 sequence and
// IntN draws for a few bounds.
func TestSplitMixReference(t *testing.T) {
	s := newSplitMix(0)
	if got := s.Uint64(); got != 0xe220a8397b1dcdaf {
		t.Fatalf("seed 0: first output %#x", got)
	}
	for _, n := range []int{1, 2, 7, 100, 1 << 40} {
		r := newSplitMix(uint64(n))
		for range 1000 {
			if v := r.IntN(n); v < 0 || v >= n {
				t.Fatalf("IntN(%d) = %d", n, v)
			}
		}
	}
	defer func() {
		if recover() == nil {
			t.Fatal("IntN(0) did not panic")
		}
	}()
	s.IntN(0)
}

func TestPickVectors(t *testing.T) {
	got := genPickVectors()
	if os.Getenv("STEER_BLESS") == "1" {
		if err := os.WriteFile(pickVectors, []byte(got), 0o644); err != nil {
			t.Fatal(err)
		}
		return
	}
	want, err := os.ReadFile(pickVectors)
	if err != nil {
		t.Fatal(err)
	}
	if string(want) != got {
		gl, wl := strings.Split(got, "\n"), strings.Split(string(want), "\n")
		for i := range min(len(gl), len(wl)) {
			if gl[i] != wl[i] {
				t.Fatalf("%s:%d: steer now says %q, fixture %q (STEER_BLESS=1 rewrites it)", pickVectors, i+1, gl[i], wl[i])
			}
		}
		t.Fatalf("%s: steer now writes %d lines, fixture has %d", pickVectors, len(gl), len(wl))
	}
}

const (
	pickCases = 40
	pickOps   = 250
)

func genPickVectors() string {
	var b strings.Builder
	b.WriteString(`# Pick sequences from steer's balancer.RoundRobin and balancer.LeastLoaded,
# written by TestPickVectors in legacy/steer/cmd/diffdrive (STEER_BLESS=1).
# Each case drives one policy with a SplitMix64 seeded with the case seed as
# its random source.
#
#   u SEED V...            SplitMix64 outputs from SEED, in hex
#   i SEED N V...          IntN(N) draws from SEED
#   rr SEED W,W,...        a round robin over endpoints of these weights
#   p2c SEED N             power of two choices over N fresh endpoints
#   o EP NS                endpoint EP observes a latency of NS nanoseconds
#   a EP / r EP            acquire / release one request on EP
#   t NS                   the clock advances NS nanoseconds
#   p CANDS TRIED -> EP    a pick: CANDS is * (every endpoint) or a list,
#                          TRIED a list or -, EP the pick or -1
#   end
`)
	for _, seed := range []uint64{0, 1, 2026} {
		s := newSplitMix(seed)
		fmt.Fprintf(&b, "u %d", seed)
		for range 6 {
			fmt.Fprintf(&b, " %x", s.Uint64())
		}
		b.WriteByte('\n')
	}
	for _, n := range []int{1, 3, 10, 100, 65537} {
		s := newSplitMix(uint64(n) * 31)
		fmt.Fprintf(&b, "i %d %d", uint64(n)*31, n)
		for range 12 {
			fmt.Fprintf(&b, " %d", s.IntN(n))
		}
		b.WriteByte('\n')
	}
	seeds := newSplitMix(2026)
	for range pickCases {
		genRoundRobin(&b, seeds.Uint64())
	}
	for range pickCases {
		genLeastLoaded(&b, seeds.Uint64())
	}
	return b.String()
}

func list(xs []int) string {
	if len(xs) == 0 {
		return "-"
	}
	s := make([]string, len(xs))
	for i, x := range xs {
		s[i] = strconv.Itoa(x)
	}
	return strings.Join(s, ",")
}

// pickArgs draws the candidates and tried set of one pick: usually every
// endpoint and nothing tried, as on a first attempt, sometimes a subset (one
// priority level) and some already tried endpoints (a retry).
func pickArgs(g *splitMix, n int) (cands []int, all bool, tried []int) {
	all = g.IntN(10) < 7
	for i := range n {
		if all || g.IntN(10) < 6 {
			cands = append(cands, i)
		}
	}
	if g.IntN(10) < 3 {
		for range 1 + g.IntN(min(3, n)) {
			tried = append(tried, g.IntN(n))
		}
	}
	return cands, all, tried
}

func writePick(b *strings.Builder, cands []int, all bool, tried []int, got int) {
	c := list(cands)
	if all {
		c = "*"
	}
	fmt.Fprintf(b, "p %s %s -> %d\n", c, list(tried), got)
}

func skipOf(tried []int) func(int) bool {
	return func(i int) bool {
		for _, j := range tried {
			if i == j {
				return true
			}
		}
		return false
	}
}

func genRoundRobin(b *strings.Builder, seed uint64) {
	g := newSplitMix(seed ^ 0x5eed)
	n := 1 + g.IntN(12)
	weights := make([]int, n)
	eps := make([]*endpoint.Endpoint, n)
	for i := range n {
		weights[i] = 1 + g.IntN(10)
		if g.IntN(10) == 0 {
			weights[i] = 1 + g.IntN(1000)
		}
		eps[i] = endpoint.New(fmt.Sprintf("10.0.0.%d:80", i+1), weights[i], 0)
	}
	fmt.Fprintf(b, "rr %d %s\n", seed, list(weights))
	rr := balancer.NewRoundRobin(eps)
	for range pickOps {
		cands, all, tried := pickArgs(g, n)
		writePick(b, cands, all, tried, rr.Pick(cands, skipOf(tried), 0, false))
	}
	b.WriteString("end\n")
}

func genLeastLoaded(b *strings.Builder, seed uint64) {
	g := newSplitMix(seed ^ 0x5eed)
	n := 1 + g.IntN(12)
	eps := make([]*endpoint.Endpoint, n)
	for i := range n {
		eps[i] = endpoint.New(fmt.Sprintf("10.0.0.%d:80", i+1), 1, 0)
	}
	fmt.Fprintf(b, "p2c %d %d\n", seed, n)
	clk := clock.NewFake(time.Unix(1_700_000_000, 0))
	ll := balancer.NewLeastLoaded(eps, newSplitMix(seed).IntN, clk)
	inflight := make([]int, n)
	observe := func(i int) {
		d := time.Duration(1_000 + g.IntN(int(300*time.Millisecond)))
		if g.IntN(20) == 0 {
			d = time.Second // a failure penalty
		}
		eps[i].ObserveLatency(d, clk.Now())
		fmt.Fprintf(b, "o %d %d\n", i, d)
	}
	for i := range n {
		if g.IntN(10) < 7 {
			observe(i)
		}
	}
	for range pickOps {
		switch r := g.IntN(100); {
		case r < 45:
			cands, all, tried := pickArgs(g, n)
			writePick(b, cands, all, tried, ll.Pick(cands, skipOf(tried), 0, false))
		case r < 65:
			observe(g.IntN(n))
		case r < 78:
			i := g.IntN(n)
			eps[i].Acquire()
			inflight[i]++
			fmt.Fprintf(b, "a %d\n", i)
		case r < 88:
			i := g.IntN(n)
			if inflight[i] > 0 {
				eps[i].Release()
				inflight[i]--
				fmt.Fprintf(b, "r %d\n", i)
			}
		default:
			d := time.Duration(g.IntN(int(3 * time.Second)))
			clk.Advance(d)
			fmt.Fprintf(b, "t %d\n", d)
		}
	}
	b.WriteString("end\n")
}
