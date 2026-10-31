package retry

import (
	"math/rand/v2"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/clock"
)

var epoch = time.Unix(50_000, 0)

func TestBudgetFloorAndRatio(t *testing.T) {
	clk := clock.NewFake(epoch)
	b := NewBudget(0.2, 1, 10*time.Second, clk)
	// Floor: 1/s x 10 s = 10 retries with no traffic at all.
	for i := 0; i < 10; i++ {
		if !b.TryWithdraw() {
			t.Fatalf("floor retry %d refused", i)
		}
	}
	if b.TryWithdraw() {
		t.Fatal("retry beyond the floor allowed")
	}
	// 100 requests buy 20 more.
	for range 100 {
		b.Deposit()
	}
	if got := b.Balance(); got != 20 {
		t.Fatalf("balance %v want 20", got)
	}
	for i := 0; i < 20; i++ {
		if !b.TryWithdraw() {
			t.Fatalf("ratio retry %d refused", i)
		}
	}
	if b.TryWithdraw() {
		t.Fatal("retry beyond the ratio allowed")
	}
	// Everything ages out of the window.
	clk.Advance(10 * time.Second)
	if got := b.Balance(); got != 10 {
		t.Fatalf("balance after window %v want 10", got)
	}
}

func TestBudgetZero(t *testing.T) {
	b := NewBudget(0, 0, time.Second, nil)
	b.Deposit()
	if b.TryWithdraw() {
		t.Fatal("zero budget allowed a retry")
	}
	if NewBudget(0, 1, 0, nil).Balance() != 1 {
		t.Fatal("window below one second should round up to one second")
	}
}

// Against a reference that keeps every event and counts the ones whose
// second lies inside the window.
func TestBudgetMatchesReference(t *testing.T) {
	rng := rand.New(rand.NewPCG(5, 8))
	for iter := 0; iter < 200; iter++ {
		window := 1 + rng.IntN(10)
		ratio := float64(rng.IntN(5)) / 10
		minPerSec := rng.IntN(3)
		clk := clock.NewFake(epoch.Add(time.Duration(rng.IntN(1000)) * time.Millisecond))
		b := NewBudget(ratio, minPerSec, time.Duration(window)*time.Second, clk)
		type event struct {
			sec   int64
			retry bool
		}
		var events []event
		for step := 0; step < 2000; step++ {
			now := clk.Now().Unix()
			switch r := rng.IntN(10); {
			case r < 5:
				b.Deposit()
				events = append(events, event{now, false})
			case r < 9:
				var reqs, rets float64
				for _, e := range events {
					if e.sec > now-int64(window) {
						if e.retry {
							rets++
						} else {
							reqs++
						}
					}
				}
				want := rets < float64(minPerSec*window)+ratio*reqs
				if got := b.TryWithdraw(); got != want {
					t.Fatalf("iter %d step %d: TryWithdraw %v, reference %v", iter, step, got, want)
				}
				if want {
					events = append(events, event{now, true})
				}
			default:
				clk.Advance(time.Duration(rng.IntN(2500)) * time.Millisecond)
			}
		}
	}
}

func TestBackoff(t *testing.T) {
	top := func() float64 { return 0.999999 }
	cases := []struct {
		attempt int
		want    time.Duration
	}{{0, 25 * time.Millisecond}, {1, 50 * time.Millisecond}, {2, 100 * time.Millisecond}, {3, 200 * time.Millisecond},
		{4, 250 * time.Millisecond}, {40, 250 * time.Millisecond}, {62, 250 * time.Millisecond}}
	for _, c := range cases {
		got := Backoff(c.attempt, 25*time.Millisecond, 250*time.Millisecond, top)
		if got > c.want || got < c.want*99/100 {
			t.Errorf("attempt %d: %v want just under %v", c.attempt, got, c.want)
		}
	}
	if Backoff(3, time.Millisecond, time.Second, func() float64 { return 0 }) != 0 {
		t.Fatal("full jitter must reach zero")
	}
}

func BenchmarkBudget(b *testing.B) {
	bud := NewBudget(0.2, 10, 10*time.Second, nil)
	b.RunParallel(func(pb *testing.PB) {
		for pb.Next() {
			bud.Deposit()
			bud.TryWithdraw()
		}
	})
}
