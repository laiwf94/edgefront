package endpoint

import (
	"math"
	"sync"
	"testing"
	"time"
)

var epoch = time.Unix(1_000, 0)

func TestPeakEWMA(t *testing.T) {
	s := NewState()
	if s.Latency(epoch) != 0 || s.Cost(epoch) != 1e-6 {
		t.Fatalf("fresh state latency %v cost %v", s.Latency(epoch), s.Cost(epoch))
	}
	s.ObserveLatency(10*time.Millisecond, epoch)
	if s.Latency(epoch) != 0.01 {
		t.Fatalf("first sample %v", s.Latency(epoch))
	}
	t1 := epoch.Add(time.Millisecond)
	s.ObserveLatency(50*time.Millisecond, t1) // slower: jumps to the peak
	if s.Latency(t1) != 0.05 {
		t.Fatalf("peak %v", s.Latency(t1))
	}
	// A faster sample one time constant later moves 1-1/e of the way down.
	t2 := t1.Add(LatencyDecay)
	s.ObserveLatency(10*time.Millisecond, t2)
	want := 0.05/math.E + 0.01*(1-1/math.E)
	if math.Abs(s.Latency(t2)-want) > 1e-9 {
		t.Fatalf("decayed %v want %v", s.Latency(t2), want)
	}
	// Left alone, the estimate decays towards zero.
	if got := s.Latency(t2.Add(LatencyDecay)); math.Abs(got-want/math.E) > 1e-9 {
		t.Fatalf("idle decay %v want %v", got, want/math.E)
	}
	// Reading at an earlier time does not inflate the estimate.
	if s.Latency(epoch) > s.Latency(t2) {
		t.Fatal("clock skew increased the estimate")
	}
	s.Acquire()
	s.Acquire()
	if s.InFlight() != 2 || math.Abs(s.Cost(t2)-3*s.Latency(t2)) > 1e-12 {
		t.Fatalf("cost %v", s.Cost(t2))
	}
	s.Release()
	s.Release()
}

func TestRecordProbeThresholds(t *testing.T) {
	s := NewState()
	if h, c := s.RecordProbe(false, "x", 2, 2); !h || c {
		t.Fatal("one failure flipped it")
	}
	if h, c := s.RecordProbe(false, "refused", 2, 2); h || !c {
		t.Fatal("two failures did not flip it")
	}
	if snap := s.Snapshot(epoch); snap.LastProbeError != "refused" || snap.Available {
		t.Fatalf("snapshot %+v", snap)
	}
	if h, c := s.RecordProbe(false, "x", 2, 2); h || c {
		t.Fatal("already unhealthy")
	}
	s.RecordProbe(true, "", 2, 2)
	if h, c := s.RecordProbe(true, "", 2, 2); !h || !c {
		t.Fatal("two passes did not revive it")
	}
	s.SetHealthy(false)
	if s.Healthy() {
		t.Fatal("SetHealthy")
	}
}

func TestEjectionLifecycle(t *testing.T) {
	s := NewState()
	if s.Ejected(epoch) || !s.EjectedUntil().IsZero() {
		t.Fatal("fresh state ejected")
	}
	for range 3 {
		s.RecordResult(true, true)
	}
	until := s.Eject(epoch, 10*time.Second, 25*time.Second, "test")
	if !until.Equal(epoch.Add(10*time.Second)) || !s.Ejected(epoch) || s.Available(epoch) {
		t.Fatalf("until %v", until)
	}
	if snap := s.Snapshot(epoch); snap.ConsecutiveFailures != 0 || snap.LastEjectReason != "test" || snap.TimesEjected != 1 {
		t.Fatalf("snapshot %+v", snap)
	}
	if !s.EjectedUntil().Equal(until) {
		t.Fatal("EjectedUntil")
	}
	// Decay does nothing while ejected.
	s.Decay(epoch.Add(5*time.Second), 10*time.Second)
	if s.TimesEjected() != 1 {
		t.Fatal("decayed while ejected")
	}
	until = s.Eject(epoch.Add(10*time.Second), 10*time.Second, 25*time.Second, "test")
	if got := until.Sub(epoch.Add(10 * time.Second)); got != 20*time.Second {
		t.Fatalf("second ejection %v", got)
	}
	until = s.Eject(epoch.Add(30*time.Second), 10*time.Second, 25*time.Second, "test")
	if got := until.Sub(epoch.Add(30 * time.Second)); got != 25*time.Second {
		t.Fatalf("capped ejection %v", got)
	}
	s.ClearEjection()
	if s.Ejected(epoch.Add(30*time.Second)) || s.TimesEjected() != 0 {
		t.Fatal("ClearEjection")
	}
	// The multiplier cannot overflow, however many ejections pile up.
	for range 1000 {
		s.Eject(epoch, time.Nanosecond, time.Hour, "x")
	}
	if !s.EjectedUntil().After(epoch) {
		t.Fatal("overflowed")
	}
}

func TestDecay(t *testing.T) {
	const period = 10 * time.Second
	s := NewState()
	s.Decay(epoch, period) // never ejected: nothing to do
	if s.TimesEjected() != 0 {
		t.Fatal("decayed a fresh state")
	}
	for range 3 {
		s.Eject(epoch, time.Second, time.Hour, "x") // multiplier 3, ends at epoch+3s
	}
	end := epoch.Add(3 * time.Second)
	s.Decay(end.Add(period-time.Nanosecond), period)
	if s.TimesEjected() != 3 {
		t.Fatalf("partial period decayed: %d", s.TimesEjected())
	}
	s.Decay(end.Add(period), period)
	if s.TimesEjected() != 2 {
		t.Fatalf("one period: %d", s.TimesEjected())
	}
	// The unused remainder of a period still counts at the next call.
	s.Decay(end.Add(2*period+period/2), period)
	if s.TimesEjected() != 1 {
		t.Fatalf("two periods: %d", s.TimesEjected())
	}
	s.Decay(end.Add(3*period), period)
	if s.TimesEjected() != 0 {
		t.Fatalf("three periods: %d", s.TimesEjected())
	}
	s.Decay(end.Add(100*period), period) // never below zero
	if s.TimesEjected() != 0 {
		t.Fatalf("went negative: %d", s.TimesEjected())
	}
}

func TestIntervalCounters(t *testing.T) {
	s := NewState()
	s.RecordResult(false, false)
	s.RecordResult(false, true)
	if n := s.RecordResult(true, true); n != 1 {
		t.Fatalf("run %d", n)
	}
	ok, bad := s.TakeInterval()
	if ok != 1 || bad != 2 {
		t.Fatalf("ok %d bad %d", ok, bad)
	}
	if ok, bad = s.TakeInterval(); ok != 0 || bad != 0 {
		t.Fatal("not reset")
	}
}

func TestConcurrentUse(t *testing.T) {
	s := NewState()
	var wg sync.WaitGroup
	for i := 0; i < 8; i++ {
		wg.Go(func() {
			for k := 0; k < 1000; k++ {
				s.Acquire()
				s.ObserveLatency(time.Millisecond, epoch)
				s.RecordResult(k%3 == 0, k%5 == 0)
				_ = s.Snapshot(epoch)
				s.Release()
			}
		})
	}
	wg.Wait()
	if s.InFlight() != 0 {
		t.Fatalf("in flight %d", s.InFlight())
	}
	if New("a:1", 2, 3).Weight != 2 {
		t.Fatal("New")
	}
}
