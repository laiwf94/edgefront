package outlier

import (
	"context"
	"fmt"
	"math"
	"math/rand/v2"
	"sort"
	"sync"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
)

var epoch = time.Unix(10_000, 0)

func hosts(n int) []*endpoint.Endpoint {
	out := make([]*endpoint.Endpoint, n)
	for i := range out {
		out[i] = endpoint.New(fmt.Sprintf("10.0.0.%d:80", i+1), 1, 0)
	}
	return out
}

func baseConfig() config.OutlierDetection {
	return config.OutlierDetection{
		ConsecutiveGatewayFailures: 5,
		Interval:                   10 * time.Second,
		BaseEjectionTime:           30 * time.Second,
		MaxEjectionTime:            300 * time.Second,
		MaxEjectionPercent:         100,
		SuccessRateMinimumHosts:    5,
		SuccessRateRequestVolume:   100,
		SuccessRateStdevFactor:     1.9,
	}
}

type ejection struct {
	addr   string
	reason string
	until  time.Time
}

func newDetector(cfg config.OutlierDetection, eps []*endpoint.Endpoint) (*Detector, *clock.Fake, *[]ejection) {
	clk := clock.NewFake(epoch)
	var mu sync.Mutex
	log := &[]ejection{}
	d := New(cfg, eps, clk, func(ep *endpoint.Endpoint, reason string, until time.Time) {
		mu.Lock()
		defer mu.Unlock()
		*log = append(*log, ejection{ep.Address, reason, until})
	})
	return d, clk, log
}

func TestConsecutiveGatewayFailures(t *testing.T) {
	eps := hosts(3)
	d, clk, log := newDetector(baseConfig(), eps)
	for range 4 {
		d.Report(eps[0], true, true)
	}
	d.Report(eps[0], false, false) // a success resets the run
	for range 4 {
		d.Report(eps[0], true, true)
	}
	if eps[0].Ejected(clk.Now()) {
		t.Fatal("ejected before 5 in a row")
	}
	d.Report(eps[0], false, true) // a plain 500 is not a gateway failure and resets the run
	for range 5 {
		d.Report(eps[0], true, true)
	}
	if !eps[0].Ejected(clk.Now()) || len(*log) != 1 || (*log)[0].reason != ReasonConsecutiveGatewayFailure {
		t.Fatalf("not ejected: %+v", *log)
	}
	if want := epoch.Add(30 * time.Second); !(*log)[0].until.Equal(want) {
		t.Fatalf("until %v want %v", (*log)[0].until, want)
	}
	clk.Advance(30 * time.Second)
	if eps[0].Ejected(clk.Now()) {
		t.Fatal("still ejected after base ejection time")
	}
	// Second ejection lasts twice as long.
	for range 5 {
		d.Report(eps[0], true, true)
	}
	if want := clk.Now().Add(60 * time.Second); !(*log)[1].until.Equal(want) {
		t.Fatalf("second ejection until %v want %v", (*log)[1].until, want)
	}
}

func TestEjectionTimeIsCapped(t *testing.T) {
	cfg := baseConfig()
	cfg.MaxEjectionTime = 70 * time.Second
	eps := hosts(1)
	d, clk, log := newDetector(cfg, eps)
	for i := 0; i < 5; i++ {
		for range 5 {
			d.Report(eps[0], true, true)
		}
		clk.Advance(5 * time.Minute)
	}
	want := []time.Duration{30, 60, 70, 70, 70}
	for i, e := range *log {
		start := epoch.Add(time.Duration(i) * 5 * time.Minute)
		if got := e.until.Sub(start); got != want[i]*time.Second {
			t.Fatalf("ejection %d lasted %v want %vs", i, got, want[i])
		}
	}
}

func TestMultiplierDecaysWhileHealthy(t *testing.T) {
	eps := hosts(1)
	d, clk, _ := newDetector(baseConfig(), eps)
	eject := func() {
		for range 5 {
			d.Report(eps[0], true, true)
		}
	}
	eject()
	clk.Advance(30 * time.Second)
	eject()
	if eps[0].TimesEjected() != 2 {
		t.Fatalf("times %d", eps[0].TimesEjected())
	}
	clk.Advance(60*time.Second + 29*time.Second) // ejection over, 29s healthy
	d.Tick()
	if eps[0].TimesEjected() != 2 {
		t.Fatal("decayed before a full base period")
	}
	clk.Advance(time.Second)
	d.Tick()
	if eps[0].TimesEjected() != 1 {
		t.Fatalf("times %d after one healthy period", eps[0].TimesEjected())
	}
	clk.Advance(10 * time.Minute)
	d.Tick()
	if eps[0].TimesEjected() != 0 {
		t.Fatalf("times %d after a long healthy period", eps[0].TimesEjected())
	}
}

func TestMaxEjectionPercent(t *testing.T) {
	cfg := baseConfig()
	cfg.MaxEjectionPercent = 40
	eps := hosts(5) // at most 2 of 5
	d, clk, _ := newDetector(cfg, eps)
	for _, ep := range eps {
		for range 5 {
			d.Report(ep, true, true)
		}
	}
	ejected := 0
	for _, ep := range eps {
		if ep.Ejected(clk.Now()) {
			ejected++
		}
	}
	if ejected != 2 {
		t.Fatalf("ejected %d of 5 with a 40%% cap", ejected)
	}
	// A low percent still allows one ejection.
	cfg.MaxEjectionPercent = 1
	eps = hosts(3)
	d, clk, _ = newDetector(cfg, eps)
	for _, ep := range eps {
		for range 5 {
			d.Report(ep, true, true)
		}
	}
	if !eps[0].Ejected(clk.Now()) || eps[1].Ejected(clk.Now()) {
		t.Fatal("expected exactly the first host ejected")
	}
	// Zero disables ejection.
	cfg.MaxEjectionPercent = 0
	eps = hosts(3)
	d, clk, _ = newDetector(cfg, eps)
	for range 10 {
		d.Report(eps[0], true, true)
	}
	if eps[0].Ejected(clk.Now()) {
		t.Fatal("ejected with max_ejection_percent 0")
	}
}

func TestConsecutiveDisabled(t *testing.T) {
	cfg := baseConfig()
	cfg.ConsecutiveGatewayFailures = 0
	eps := hosts(2)
	d, clk, _ := newDetector(cfg, eps)
	for range 100 {
		d.Report(eps[0], true, true)
	}
	if eps[0].Ejected(clk.Now()) {
		t.Fatal("ejected with the detector disabled")
	}
}

// Five hosts, one failing half its requests: rates 1,1,1,1,0.5 give mean 0.9,
// stdev 0.2, threshold 0.9 - 1.9 x 0.2 = 0.52.
func TestSuccessRateEjection(t *testing.T) {
	cfg := baseConfig()
	cfg.ConsecutiveGatewayFailures = 0
	eps := hosts(5)
	d, clk, log := newDetector(cfg, eps)
	for i, ep := range eps {
		for k := 0; k < 200; k++ {
			fail := i == 4 && k%2 == 0
			d.Report(ep, false, fail)
		}
	}
	d.Tick()
	if len(*log) != 1 || (*log)[0].addr != eps[4].Address || (*log)[0].reason != ReasonSuccessRate {
		t.Fatalf("ejections %+v", *log)
	}
	if !eps[4].Ejected(clk.Now()) {
		t.Fatal("not ejected")
	}
	// Counters were reset: a quiet interval ejects nobody new.
	d.Tick()
	if len(*log) != 1 {
		t.Fatalf("ejections after a quiet interval %+v", *log)
	}
}

func TestSuccessRateNeedsVolumeAndHosts(t *testing.T) {
	cfg := baseConfig()
	cfg.ConsecutiveGatewayFailures = 0
	eps := hosts(5)
	d, _, log := newDetector(cfg, eps)
	for i, ep := range eps {
		n := 200
		if i == 0 {
			n = 50 // below volume: four eligible hosts < minimum of five
		}
		for k := 0; k < n; k++ {
			d.Report(ep, false, i == 4 && k%2 == 0)
		}
	}
	d.Tick()
	if len(*log) != 0 {
		t.Fatalf("ejected with too few eligible hosts: %+v", *log)
	}
	// Uniformly bad hosts are not outliers.
	for _, ep := range eps {
		for k := 0; k < 200; k++ {
			d.Report(ep, false, k%2 == 0)
		}
	}
	d.Tick()
	if len(*log) != 0 {
		t.Fatalf("ejected with zero variance: %+v", *log)
	}
	// Disabled by minimum hosts 0.
	cfg.SuccessRateMinimumHosts = 0
	d, _, log = newDetector(cfg, eps)
	for i, ep := range eps {
		for k := 0; k < 200; k++ {
			d.Report(ep, false, i == 4)
		}
	}
	d.Tick()
	if len(*log) != 0 {
		t.Fatalf("ejected with success rate disabled: %+v", *log)
	}
}

func TestRunTicksOnTheClock(t *testing.T) {
	cfg := baseConfig()
	cfg.ConsecutiveGatewayFailures = 0
	eps := hosts(5)
	d, clk, log := newDetector(cfg, eps)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { d.Run(ctx); close(done) }()
	for i, ep := range eps {
		for k := 0; k < 200; k++ {
			d.Report(ep, false, i == 2 && k%2 == 0)
		}
	}
	if !clk.WaitForWaiters(1, 5*time.Second) {
		t.Fatal("Run never waited on the clock")
	}
	clk.Advance(cfg.Interval)
	deadline := time.Now().Add(5 * time.Second)
	for !eps[2].Ejected(clk.Now()) {
		if time.Now().After(deadline) {
			t.Fatalf("no ejection after an interval: %+v", *log)
		}
		time.Sleep(time.Millisecond)
	}
	cancel()
	<-done
}

// refModel re-states the rules in the plainest possible form: per-host
// arrays, recomputed from scratch, no shared code with the detector.
type refModel struct {
	cfg   config.OutlierDetection
	now   time.Time
	run   []int
	ok    []uint64
	bad   []uint64
	times []int
	until []time.Time
	decay []time.Time
}

func newRef(cfg config.OutlierDetection, n int) *refModel {
	return &refModel{cfg: cfg, now: epoch, run: make([]int, n), ok: make([]uint64, n), bad: make([]uint64, n),
		times: make([]int, n), until: make([]time.Time, n), decay: make([]time.Time, n)}
}

func (m *refModel) ejected(i int) bool { return m.now.Before(m.until[i]) }

func (m *refModel) mayEject() bool {
	n := len(m.run)
	limit := n * m.cfg.MaxEjectionPercent / 100
	if m.cfg.MaxEjectionPercent > 0 && limit == 0 {
		limit = 1
	}
	count := 0
	for i := 0; i < n; i++ {
		if m.ejected(i) {
			count++
		}
	}
	return count < limit
}

func (m *refModel) eject(i int) {
	m.times[i]++
	d := m.cfg.BaseEjectionTime * time.Duration(m.times[i])
	if d > m.cfg.MaxEjectionTime {
		d = m.cfg.MaxEjectionTime
	}
	m.until[i] = m.now.Add(d)
	m.decay[i] = m.until[i]
	m.run[i] = 0
}

func (m *refModel) report(i int, gw, fail bool) {
	if gw {
		m.run[i]++
	} else {
		m.run[i] = 0
	}
	if gw || fail {
		m.bad[i]++
	} else {
		m.ok[i]++
	}
	if m.cfg.ConsecutiveGatewayFailures > 0 && m.run[i] >= m.cfg.ConsecutiveGatewayFailures && !m.ejected(i) && m.mayEject() {
		m.eject(i)
	}
}

func (m *refModel) tick() {
	type cand struct {
		i    int
		rate float64
	}
	var cands []cand
	for i := range m.run {
		if m.times[i] > 0 && !m.ejected(i) {
			for m.times[i] > 0 && m.now.Sub(m.decay[i]) >= m.cfg.BaseEjectionTime {
				m.times[i]--
				m.decay[i] = m.decay[i].Add(m.cfg.BaseEjectionTime)
			}
		}
		total := m.ok[i] + m.bad[i]
		if m.cfg.SuccessRateMinimumHosts > 0 && !m.ejected(i) && total >= uint64(m.cfg.SuccessRateRequestVolume) {
			cands = append(cands, cand{i, float64(m.ok[i]) / float64(total)})
		}
		m.ok[i], m.bad[i] = 0, 0
	}
	if m.cfg.SuccessRateMinimumHosts == 0 || len(cands) < m.cfg.SuccessRateMinimumHosts {
		return
	}
	mean := 0.0
	for _, c := range cands {
		mean += c.rate
	}
	mean /= float64(len(cands))
	v := 0.0
	for _, c := range cands {
		v += (c.rate - mean) * (c.rate - mean)
	}
	sd := math.Sqrt(v / float64(len(cands)))
	if sd < 1e-9 {
		return
	}
	sort.SliceStable(cands, func(a, b int) bool { return cands[a].rate < cands[b].rate })
	for _, c := range cands {
		if c.rate < mean-m.cfg.SuccessRateStdevFactor*sd {
			if !m.mayEject() {
				return
			}
			m.eject(c.i)
		}
	}
}

// The detector must agree with the reference model, ejection by ejection,
// over long random sequences of results, clock advances and interval ticks.
func TestMatchesReferenceModel(t *testing.T) {
	rng := rand.New(rand.NewPCG(7, 11))
	reasons := map[string]int{}
	for iter := 0; iter < 400; iter++ {
		n := 1 + rng.IntN(7)
		cfg := config.OutlierDetection{
			ConsecutiveGatewayFailures: rng.IntN(5),
			Interval:                   time.Second,
			BaseEjectionTime:           time.Duration(1+rng.IntN(20)) * time.Second,
			MaxEjectionPercent:         []int{0, 10, 34, 50, 100}[rng.IntN(5)],
			SuccessRateMinimumHosts:    rng.IntN(4),
			SuccessRateRequestVolume:   1 + rng.IntN(10),
			SuccessRateStdevFactor:     0.5 + rng.Float64()*1.5,
		}
		cfg.MaxEjectionTime = cfg.BaseEjectionTime * time.Duration(1+rng.IntN(5))
		eps := hosts(n)
		d, clk, log := newDetector(cfg, eps)
		ref := newRef(cfg, n)
		badness := make([]float64, n)
		for i := range badness {
			badness[i] = []float64{0, 0.05, 0.5, 0.95}[rng.IntN(4)]
		}
		for step := 0; step < 400; step++ {
			switch r := rng.IntN(20); {
			case r < 16:
				i := rng.IntN(n)
				fail := rng.Float64() < badness[i]
				gw := fail && rng.IntN(4) != 0
				d.Report(eps[i], gw, fail)
				ref.report(i, gw, fail)
			case r < 18:
				dt := time.Duration(rng.IntN(15000)) * time.Millisecond
				clk.Advance(dt)
				ref.now = ref.now.Add(dt)
			default:
				d.Tick()
				ref.tick()
			}
			for i, ep := range eps {
				if got, want := ep.Ejected(clk.Now()), ref.ejected(i); got != want {
					t.Fatalf("iter %d step %d host %d: ejected %v, reference %v (cfg %+v)", iter, step, i, got, want, cfg)
				}
				if got, want := ep.TimesEjected(), ref.times[i]; got != want {
					t.Fatalf("iter %d step %d host %d: times ejected %d, reference %d (cfg %+v)", iter, step, i, got, want, cfg)
				}
			}
		}
		for _, e := range *log {
			reasons[e.reason]++
		}
	}
	// The sequences must actually exercise both detectors.
	if reasons[ReasonConsecutiveGatewayFailure] < 100 || reasons[ReasonSuccessRate] < 100 {
		t.Fatalf("too few ejections to be meaningful: %v", reasons)
	}
	t.Logf("ejections: %v", reasons)
}

func BenchmarkReport(b *testing.B) {
	eps := hosts(10)
	d := New(baseConfig(), eps, nil, nil)
	b.RunParallel(func(pb *testing.PB) {
		i := 0
		for pb.Next() {
			d.Report(eps[i%10], false, false)
			i++
		}
	})
}
