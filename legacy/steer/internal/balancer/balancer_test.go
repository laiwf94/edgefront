package balancer

import (
	"fmt"
	"math"
	"math/rand/v2"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
)

func eps(specs ...[2]int) []*endpoint.Endpoint { // {weight, priority}
	out := make([]*endpoint.Endpoint, len(specs))
	for i, s := range specs {
		out[i] = endpoint.New(fmt.Sprintf("10.0.0.%d:80", i+1), s[0], s[1])
	}
	return out
}

func noSkip(int) bool { return false }

// Examples from Envoy's priority-levels documentation (overprovisioning 1.4).
func TestPriorityLoadEnvoyExamples(t *testing.T) {
	cases := []struct {
		p0, p1     int // healthy percent of 100 hosts
		load0, ld1 int
	}{
		{100, 100, 100, 0},
		{72, 100, 100, 0},
		{71, 100, 99, 1},
		{50, 100, 70, 30},
		{25, 100, 35, 65},
		{25, 25, 50, 50},
		{0, 100, 0, 100},
	}
	for _, c := range cases {
		load, panic := PriorityLoad([]int{c.p0, c.p1}, []int{100, 100}, 0)
		if panic || load[0] != c.load0 || load[1] != c.ld1 {
			t.Errorf("P0 %d%% P1 %d%%: load %v panic %v, want [%d %d]", c.p0, c.p1, load, panic, c.load0, c.ld1)
		}
	}
}

func TestPriorityLoadPanicAndEmpty(t *testing.T) {
	// 20% healthy in the only level: normalized health 28 < 50.
	if _, panic := PriorityLoad([]int{1}, []int{5}, 50); !panic {
		t.Fatal("expected panic")
	}
	if load, panic := PriorityLoad([]int{0, 0}, []int{2, 1}, 0); panic || load != nil {
		t.Fatalf("no healthy hosts, panic disabled: load %v panic %v", load, panic)
	}
	if _, panic := PriorityLoad([]int{0}, []int{3}, 1); !panic {
		t.Fatal("all down with a threshold must panic")
	}
	// A level with no hosts contributes nothing.
	load, _ := PriorityLoad([]int{0, 2, 1}, []int{0, 2, 1}, 50)
	if !slices.Equal(load, []int{0, 100, 0}) {
		t.Fatalf("empty P0: %v", load)
	}
	// Rounding leftovers go to the first level with health: 1/3 of P0 healthy
	// (health 46), 1/3 of P1 healthy (46): 92 normalised.
	load, _ = PriorityLoad([]int{1, 1}, []int{3, 3}, 0)
	if load[0]+load[1] != 100 || load[0] < load[1] {
		t.Fatalf("rounding: %v", load)
	}
}

// Randomised: loads always sum to 100, are never negative, follow priority
// order, and agree with a floating-point reference within rounding.
func TestPriorityLoadProperties(t *testing.T) {
	rng := rand.New(rand.NewPCG(1, 2))
	for iter := 0; iter < 20000; iter++ {
		levels := 1 + rng.IntN(4)
		healthy, total := make([]int, levels), make([]int, levels)
		for p := range total {
			total[p] = rng.IntN(6)
			if total[p] > 0 {
				healthy[p] = rng.IntN(total[p] + 1)
			}
		}
		load, panic := PriorityLoad(healthy, total, 0)
		// Reference with real numbers.
		health := make([]float64, levels)
		sum := 0.0
		for p := range total {
			if total[p] > 0 {
				health[p] = math.Min(100, math.Floor(140*float64(healthy[p])/float64(total[p])))
			}
			sum += health[p]
		}
		if panic {
			t.Fatalf("panic with threshold 0")
		}
		if sum == 0 {
			if load != nil {
				t.Fatalf("no health but load %v", load)
			}
			continue
		}
		norm := math.Min(100, sum)
		got := 0
		remaining := 100.0
		for p := range load {
			if load[p] < 0 {
				t.Fatalf("negative load %v", load)
			}
			want := math.Min(remaining, math.Floor(health[p]*100/norm))
			remaining -= want
			if health[p] == 0 && load[p] != 0 {
				t.Fatalf("level %d has no health but load %v (healthy %v total %v)", p, load, healthy, total)
			}
			if d := float64(load[p]) - want; d < 0 || d > float64(levels) {
				t.Fatalf("level %d: load %d, reference %.0f (healthy %v total %v)", p, load[p], want, healthy, total)
			}
			got += load[p]
		}
		if got != 100 {
			t.Fatalf("loads %v sum to %d", load, got)
		}
	}
}

func TestSmoothRoundRobinSequence(t *testing.T) {
	e := eps([2]int{5, 0}, [2]int{1, 0}, [2]int{1, 0})
	rr := NewRoundRobin(e)
	var got strings.Builder
	for range 7 {
		got.WriteByte("abc"[rr.Pick([]int{0, 1, 2}, noSkip, 0, false)])
	}
	if got.String() != "aabacaa" {
		t.Fatalf("sequence %q, want aabacaa (nginx smooth weighted round robin)", got.String())
	}
}

// Against a reference model: in every window of sum(weights) consecutive
// picks over a fixed set, each endpoint is chosen exactly weight times, and
// no endpoint waits more than twice its fair interval between turns.
func TestSmoothRoundRobinMatchesReference(t *testing.T) {
	rng := rand.New(rand.NewPCG(3, 4))
	for iter := 0; iter < 300; iter++ {
		n := 1 + rng.IntN(8)
		specs := make([][2]int, n)
		total := 0
		for i := range specs {
			specs[i] = [2]int{1 + rng.IntN(10), 0}
			total += specs[i][0]
		}
		e := eps(specs...)
		rr := NewRoundRobin(e)
		cands := make([]int, n)
		for i := range cands {
			cands[i] = i
		}
		last := make([]int, n)
		for i := range last {
			last[i] = -1
		}
		for window := 0; window < 4; window++ {
			counts := make([]int, n)
			for k := 0; k < total; k++ {
				pos := window*total + k
				i := rr.Pick(cands, noSkip, 0, false)
				counts[i]++
				if last[i] >= 0 && pos-last[i] > 2*((total+specs[i][0]-1)/specs[i][0]) {
					t.Fatalf("weights %v: endpoint %d waited %d picks", specs, i, pos-last[i])
				}
				last[i] = pos
			}
			for i := range counts {
				if counts[i] != specs[i][0] {
					t.Fatalf("weights %v window %d: counts %v", specs, window, counts)
				}
			}
		}
	}
}

func TestRoundRobinSkip(t *testing.T) {
	e := eps([2]int{1, 0}, [2]int{1, 0})
	rr := NewRoundRobin(e)
	for range 10 {
		if got := rr.Pick([]int{0, 1}, func(i int) bool { return i == 0 }, 0, false); got != 1 {
			t.Fatalf("picked skipped endpoint %d", got)
		}
	}
	if got := rr.Pick([]int{0, 1}, func(int) bool { return true }, 0, false); got != -1 {
		t.Fatalf("all skipped: %d", got)
	}
}

func TestLeastLoadedPrefersCheaper(t *testing.T) {
	e := eps([2]int{1, 0}, [2]int{1, 0}, [2]int{1, 0})
	now := time.Unix(0, 0)
	e[0].ObserveLatency(100*time.Millisecond, now)
	e[1].ObserveLatency(time.Millisecond, now)
	e[2].ObserveLatency(time.Millisecond, now)
	for range 200 {
		e[2].Acquire()
	}
	ll := NewLeastLoaded(e, rand.IntN, clock.NewFake(now))
	counts := make([]int, 3)
	for range 3000 {
		counts[ll.Pick([]int{0, 1, 2}, noSkip, 0, false)]++
	}
	// Costs: 0 = 0.1 x 1, 1 = 0.001 x 1, 2 = 0.001 x 201. Endpoint 1 wins every
	// pair it is in (2/3 of picks) and 0 beats 2.
	if counts[1] < 1800 || counts[0] == 0 {
		t.Fatalf("counts %v", counts)
	}
	if counts[2] != 0 {
		t.Fatalf("most expensive endpoint picked %d times", counts[2])
	}
}

// A penalised endpoint that gets no traffic is not starved forever: its
// estimate decays until it is cheap enough to be tried again.
func TestLeastLoadedRetriesPenalisedEndpoint(t *testing.T) {
	e := eps([2]int{1, 0}, [2]int{1, 0})
	clk := clock.NewFake(time.Unix(0, 0))
	e[0].ObserveLatency(time.Second, clk.Now()) // a failure penalty
	e[1].ObserveLatency(time.Millisecond, clk.Now())
	ll := NewLeastLoaded(e, rand.IntN, clk)
	for range 100 {
		if ll.Pick([]int{0, 1}, noSkip, 0, false) != 1 {
			t.Fatal("picked the penalised endpoint right away")
		}
	}
	e[1].ObserveLatency(time.Millisecond, clk.Now())
	clk.Advance(80 * time.Second) // 1 s x e^-8 = 0.34 ms, below the fresh 1 ms of endpoint 1
	e[1].ObserveLatency(time.Millisecond, clk.Now())
	if ll.Pick([]int{0, 1}, noSkip, 0, false) != 0 {
		t.Fatal("penalised endpoint still starved after its estimate decayed")
	}
}

func TestLeastLoadedSkipAndSmallSets(t *testing.T) {
	e := eps([2]int{1, 0}, [2]int{1, 0}, [2]int{1, 0})
	ll := NewLeastLoaded(e, rand.IntN, clock.Real{})
	for range 50 {
		got := ll.Pick([]int{0, 1, 2}, func(i int) bool { return i != 2 }, 0, false)
		if got != 2 {
			t.Fatalf("got %d", got)
		}
		got = ll.Pick([]int{0, 1, 2}, func(i int) bool { return i == 1 }, 0, false)
		if got == 1 {
			t.Fatal("picked skipped")
		}
	}
	if ll.Pick([]int{0}, noSkip, 0, false) != 0 || ll.Pick(nil, noSkip, 0, false) != -1 {
		t.Fatal("small sets")
	}
}

func TestMaglevSpreadsByWeight(t *testing.T) {
	e := eps([2]int{1, 0}, [2]int{1, 0}, [2]int{2, 0}, [2]int{4, 0})
	m, err := NewMaglev(e, DefaultMaglevTableSize, rand.IntN)
	if err != nil {
		t.Fatal(err)
	}
	table := m.Populate([]int{0, 1, 2, 3})
	counts := make([]int, 4)
	for _, x := range table {
		counts[x]++
	}
	want := []float64{1.0 / 8, 1.0 / 8, 2.0 / 8, 4.0 / 8}
	for i := range counts {
		share := float64(counts[i]) / float64(len(table))
		if math.Abs(share-want[i]) > 0.01 {
			t.Fatalf("endpoint %d share %.4f want %.4f (counts %v)", i, share, want[i], counts)
		}
	}
}

// Removing one endpoint of ten must move that endpoint's keys and almost no
// others; a mod-N hash would move about 90% of all keys.
func TestMaglevMinimalDisruption(t *testing.T) {
	specs := make([][2]int, 10)
	for i := range specs {
		specs[i] = [2]int{1, 0}
	}
	e := eps(specs...)
	m, _ := NewMaglev(e, DefaultMaglevTableSize, rand.IntN)
	before := m.Populate([]int{0, 1, 2, 3, 4, 5, 6, 7, 8, 9})
	after := m.Populate([]int{0, 1, 2, 3, 4, 6, 7, 8, 9}) // endpoint 5 removed
	moved, movedOthers, owned := 0, 0, 0
	for s := range before {
		if before[s] == 5 {
			owned++
		}
		if before[s] != after[s] {
			moved++
			if before[s] != 5 {
				movedOthers++
			}
		}
		if after[s] == 5 {
			t.Fatal("removed endpoint still in table")
		}
	}
	size := float64(len(before))
	t.Logf("removed endpoint owned %.1f%% of slots; %.1f%% of slots moved; %.2f%% moved between surviving endpoints",
		100*float64(owned)/size, 100*float64(moved)/size, 100*float64(movedOthers)/size)
	if float64(movedOthers)/size > 0.03 {
		t.Fatalf("%.2f%% of slots moved between surviving endpoints", 100*float64(movedOthers)/size)
	}
	// Mod-N comparison over keys, for the record.
	modMoved := 0
	for k := 0; k < 10000; k++ {
		h := HashKey(fmt.Sprint("user-", k))
		if h%10 != h%9 {
			modMoved++
		}
	}
	if modMoved < 8000 {
		t.Fatalf("mod-N reference moved only %d of 10000", modMoved)
	}
}

func TestMaglevKeysAreStableAndRetriesWalk(t *testing.T) {
	e := eps([2]int{1, 0}, [2]int{1, 0}, [2]int{1, 0})
	m, _ := NewMaglev(e, 101, rand.IntN)
	cands := []int{0, 1, 2}
	k := HashKey("alice")
	first := m.Pick(cands, noSkip, k, true)
	for range 10 {
		if m.Pick(cands, noSkip, k, true) != first {
			t.Fatal("same key picked different endpoints")
		}
	}
	second := m.Pick(cands, func(i int) bool { return i == first }, k, true)
	if second == first || second < 0 {
		t.Fatalf("retry picked %d after %d", second, first)
	}
	if m.Pick(cands, func(i int) bool { return i == first }, k, true) != second {
		t.Fatal("retry choice not deterministic")
	}
	if m.Pick(cands, func(int) bool { return true }, k, true) != -1 {
		t.Fatal("all skipped")
	}
	// Without a key: random but never skipped.
	for range 20 {
		if got := m.Pick(cands, func(i int) bool { return i == 1 }, 0, false); got == 1 || got < 0 {
			t.Fatalf("keyless pick %d", got)
		}
	}
	if m.Pick(cands, func(int) bool { return true }, 0, false) != -1 || m.Pick(nil, noSkip, 0, false) != -1 {
		t.Fatal("empty keyless")
	}
}

func TestMaglevTableCacheEvicts(t *testing.T) {
	specs := make([][2]int, 12)
	for i := range specs {
		specs[i] = [2]int{1, 0}
	}
	e := eps(specs...)
	m, _ := NewMaglev(e, 101, rand.IntN)
	for i := range 12 {
		m.Pick([]int{i}, noSkip, 7, true)
	}
	if len(m.cache) != maglevCacheSize || len(m.order) != maglevCacheSize {
		t.Fatalf("cache %d order %d", len(m.cache), len(m.order))
	}
	if got := m.Pick([]int{0}, noSkip, 7, true); got != 0 {
		t.Fatalf("evicted table rebuilt wrong: %d", got)
	}
	if m.Populate(nil)[0] != -1 {
		t.Fatal("empty populate")
	}
}

func TestMaglevRejectsBadSize(t *testing.T) {
	e := eps([2]int{1, 0}, [2]int{1, 0})
	for _, size := range []int{0, 1, 2, 100, 65536} {
		if _, err := NewMaglev(e, size, rand.IntN); err == nil {
			t.Errorf("size %d accepted", size)
		}
	}
	if _, err := New(e, Options{Policy: config.Maglev, MaglevTableSize: 4}); err == nil {
		t.Fatal("LB accepted a bad table size")
	}
}

func newLB(t *testing.T, e []*endpoint.Endpoint, policy config.Policy, panicThreshold int, clk clock.Clock) *LB {
	t.Helper()
	lb, err := New(e, Options{Policy: policy, PanicThreshold: panicThreshold, Clock: clk})
	if err != nil {
		t.Fatal(err)
	}
	return lb
}

func TestLBPriorityFailover(t *testing.T) {
	clk := clock.NewFake(time.Unix(1000, 0))
	e := eps([2]int{1, 0}, [2]int{1, 0}, [2]int{1, 1})
	lb := newLB(t, e, config.RoundRobin, 50, clk)
	share := func() []int {
		counts := make([]int, 3)
		for range 10000 {
			counts[lb.Pick(0, false, nil)]++
		}
		return counts
	}
	if c := share(); c[2] != 0 {
		t.Fatalf("all healthy, backup got %v", c)
	}
	e[0].SetHealthy(false) // P0 50% healthy: health 70 -> 70/30 split
	c := share()
	if c[0] != 0 || math.Abs(float64(c[2])/10000-0.30) > 0.03 {
		t.Fatalf("one primary down: %v", c)
	}
	if st := lb.State(); !slices.Equal(st.PriorityLoad, []int{70, 30}) || st.Panic {
		t.Fatalf("state %+v", st)
	}
	e[1].Eject(clk.Now(), time.Minute, time.Hour, "test") // P0 empty -> all to P1
	if c := share(); c[2] != 10000 {
		t.Fatalf("both primaries out: %v", c)
	}
	clk.Advance(2 * time.Minute) // ejection over
	if c := share(); c[1] == 0 {
		t.Fatalf("ejection expiry ignored: %v", c)
	}
}

func TestLBPanicRoutesToEveryone(t *testing.T) {
	clk := clock.NewFake(time.Unix(1000, 0))
	e := eps([2]int{1, 0}, [2]int{1, 0}, [2]int{1, 0}, [2]int{1, 0}, [2]int{1, 0})
	lb := newLB(t, e, config.RoundRobin, 50, clk)
	for i := 0; i < 4; i++ {
		e[i].SetHealthy(false) // 20% healthy: health 28 < 50
	}
	counts := make([]int, 5)
	for range 500 {
		counts[lb.Pick(0, false, nil)]++
	}
	for i, c := range counts {
		if c != 100 {
			t.Fatalf("panic should spread evenly, endpoint %d got %d (%v)", i, c, counts)
		}
	}
	if !lb.State().Panic {
		t.Fatal("state not in panic")
	}
	// Without a panic threshold the last healthy host takes everything.
	lb = newLB(t, e, config.RoundRobin, 0, clk)
	for range 50 {
		if lb.Pick(0, false, nil) != 4 {
			t.Fatal("expected the healthy host")
		}
	}
	e[4].SetHealthy(false)
	if lb.Pick(0, false, nil) != -1 {
		t.Fatal("no host is available")
	}
	if st := lb.State(); len(st.PriorityLoad) != 1 || st.PriorityLoad[0] != 0 {
		t.Fatalf("state %+v", st)
	}
}

func TestLBNeverRepeatsAndFallsBack(t *testing.T) {
	for _, policy := range []config.Policy{config.RoundRobin, config.LeastLoaded, config.Maglev} {
		e := eps([2]int{1, 0}, [2]int{1, 0}, [2]int{1, 1})
		lb := newLB(t, e, policy, 50, nil)
		for iter := 0; iter < 200; iter++ {
			key := uint64(iter) * 0x9e3779b97f4a7c15
			var tried []int
			for range 3 {
				i := lb.Pick(key, true, tried)
				if i < 0 || slices.Contains(tried, i) {
					t.Fatalf("%s: picked %d after %v", policy, i, tried)
				}
				tried = append(tried, i)
			}
			if i := lb.Pick(key, true, tried); i != -1 {
				t.Fatalf("%s: picked %d after trying all", policy, i)
			}
		}
	}
}

func TestLBHashKeepsKeyOnEndpoint(t *testing.T) {
	e := eps([2]int{1, 0}, [2]int{1, 0}, [2]int{1, 0}, [2]int{1, 1})
	lb := newLB(t, e, config.Maglev, 50, nil)
	e[0].SetHealthy(false) // P0 health 93: keys split between levels by key, not by chance
	for k := 0; k < 500; k++ {
		key := HashKey(fmt.Sprint("session-", k))
		first := lb.Pick(key, true, nil)
		for range 5 {
			if lb.Pick(key, true, nil) != first {
				t.Fatalf("key %d moved", k)
			}
		}
	}
}

func TestNewValidates(t *testing.T) {
	if _, err := New(nil, Options{}); err == nil {
		t.Fatal("empty endpoints accepted")
	}
	if _, err := New(eps([2]int{0, 0}), Options{}); err == nil {
		t.Fatal("zero weight accepted")
	}
	if _, err := New(eps([2]int{1, 0}), Options{Policy: "random"}); err == nil {
		t.Fatal("unknown policy accepted")
	}
	lb, err := New(eps([2]int{1, 0}), Options{})
	if err != nil || len(lb.Endpoints()) != 1 {
		t.Fatal(err)
	}
}

func benchmarkPick(b *testing.B, policy config.Policy, n int) {
	specs := make([][2]int, n)
	for i := range specs {
		specs[i] = [2]int{1 + i%3, 0}
	}
	lb, err := New(eps(specs...), Options{Policy: policy, PanicThreshold: 50})
	if err != nil {
		b.Fatal(err)
	}
	b.ReportAllocs()
	b.RunParallel(func(pb *testing.PB) {
		var k uint64
		for pb.Next() {
			k += 0x9e3779b97f4a7c15
			lb.Pick(k, true, nil)
		}
	})
}

func BenchmarkPickRoundRobin10(b *testing.B)   { benchmarkPick(b, config.RoundRobin, 10) }
func BenchmarkPickRoundRobin100(b *testing.B)  { benchmarkPick(b, config.RoundRobin, 100) }
func BenchmarkPickLeastLoaded10(b *testing.B)  { benchmarkPick(b, config.LeastLoaded, 10) }
func BenchmarkPickLeastLoaded100(b *testing.B) { benchmarkPick(b, config.LeastLoaded, 100) }
func BenchmarkPickMaglev10(b *testing.B)       { benchmarkPick(b, config.Maglev, 10) }
func BenchmarkPickMaglev100(b *testing.B)      { benchmarkPick(b, config.Maglev, 100) }

func BenchmarkMaglevPopulate100(b *testing.B) {
	specs := make([][2]int, 100)
	for i := range specs {
		specs[i] = [2]int{1, 0}
	}
	e := eps(specs...)
	m, _ := NewMaglev(e, DefaultMaglevTableSize, rand.IntN)
	cands := make([]int, 100)
	for i := range cands {
		cands[i] = i
	}
	for b.Loop() {
		m.Populate(cands)
	}
}
