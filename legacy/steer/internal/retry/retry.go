// Package retry provides the retry budget and backoff used by the proxy.
package retry

import (
	"sync"
	"time"

	"github.com/laiwf94/steer/internal/clock"
)

// Budget caps retries at a fraction of recent requests plus a small floor,
// in the style of Finagle's RetryBudget. Without it, an upstream outage turns
// every request into 1 + max_retries requests and the extra load keeps the
// upstream down. With ratio 0.2, retries add at most 20% load (plus the floor)
// no matter how much fails.
//
// Requests and retries are counted in one-second buckets over a sliding
// window. A retry is allowed while
//
//	retries in window < min_per_second x window_seconds + ratio x requests in window
type Budget struct {
	ratio     float64
	minPerSec int
	clk       clock.Clock

	mu      sync.Mutex
	buckets []bucket // ring indexed by unix second mod len
}

type bucket struct {
	sec               int64
	requests, retries int64
}

// NewBudget returns a budget over window (rounded down to whole seconds, at
// least one).
func NewBudget(ratio float64, minPerSecond int, window time.Duration, clk clock.Clock) *Budget {
	if clk == nil {
		clk = clock.Real{}
	}
	n := max(int(window/time.Second), 1)
	return &Budget{ratio: ratio, minPerSec: minPerSecond, clk: clk, buckets: make([]bucket, n)}
}

// current returns the bucket for now, recycling a stale one. Callers hold mu.
func (b *Budget) current() *bucket {
	sec := b.clk.Now().Unix()
	bk := &b.buckets[int(uint64(sec)%uint64(len(b.buckets)))]
	if bk.sec != sec {
		*bk = bucket{sec: sec}
	}
	return bk
}

// totals sums the buckets inside the window. Callers hold mu.
func (b *Budget) totals() (requests, retries int64) {
	now := b.clk.Now().Unix()
	for _, bk := range b.buckets {
		if bk.sec > now-int64(len(b.buckets)) && bk.sec <= now {
			requests += bk.requests
			retries += bk.retries
		}
	}
	return requests, retries
}

// Deposit records one original request.
func (b *Budget) Deposit() {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.current().requests++
}

// TryWithdraw reports whether a retry may be sent now, and records it if so.
func (b *Budget) TryWithdraw() bool {
	b.mu.Lock()
	defer b.mu.Unlock()
	requests, retries := b.totals()
	allowed := float64(b.minPerSec*len(b.buckets)) + b.ratio*float64(requests)
	if float64(retries) >= allowed {
		return false
	}
	b.current().retries++
	return true
}

// Balance returns how many more retries the budget would allow now.
func (b *Budget) Balance() float64 {
	b.mu.Lock()
	defer b.mu.Unlock()
	requests, retries := b.totals()
	return max(0, float64(b.minPerSec*len(b.buckets))+b.ratio*float64(requests)-float64(retries))
}

// Backoff returns the wait before retry number attempt (0 for the first
// retry): exponential with full jitter, a uniform draw from
// [0, min(maxWait, base x 2^attempt)). Full jitter spreads synchronised
// clients out best (Brooker, "Exponential Backoff And Jitter").
// random returns a uniform float in [0, 1).
func Backoff(attempt int, base, maxWait time.Duration, random func() float64) time.Duration {
	ceiling := maxWait
	if attempt < 32 && base<<attempt > 0 && base<<attempt < maxWait {
		ceiling = base << attempt
	}
	return time.Duration(random() * float64(ceiling))
}
