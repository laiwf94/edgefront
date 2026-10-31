// Package clock abstracts time so that health checking, outlier detection and
// retry budgets can be tested deterministically.
package clock

import (
	"sort"
	"sync"
	"time"
)

// Clock tells the time and makes timers.
type Clock interface {
	Now() time.Time
	// After returns a channel that receives the time once d has passed.
	After(d time.Duration) <-chan time.Time
}

// Real is the wall clock.
type Real struct{}

// Now returns time.Now().
func (Real) Now() time.Time { return time.Now() }

// After is time.After.
func (Real) After(d time.Duration) <-chan time.Time { return time.After(d) }

// Fake is a manually advanced clock. It is safe for concurrent use.
type Fake struct {
	mu      sync.Mutex
	now     time.Time
	waiters []waiter
}

type waiter struct {
	at time.Time
	ch chan time.Time
}

// NewFake returns a fake clock reading t.
func NewFake(t time.Time) *Fake { return &Fake{now: t} }

// Now returns the fake time.
func (f *Fake) Now() time.Time {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.now
}

// After returns a channel that fires when the clock is advanced past d.
func (f *Fake) After(d time.Duration) <-chan time.Time {
	f.mu.Lock()
	defer f.mu.Unlock()
	ch := make(chan time.Time, 1)
	if d <= 0 {
		ch <- f.now
		return ch
	}
	f.waiters = append(f.waiters, waiter{at: f.now.Add(d), ch: ch})
	return ch
}

// Advance moves the clock forward by d and fires every timer that is due, in
// order of their deadlines.
func (f *Fake) Advance(d time.Duration) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.now = f.now.Add(d)
	sort.SliceStable(f.waiters, func(i, j int) bool { return f.waiters[i].at.Before(f.waiters[j].at) })
	n := 0
	for _, w := range f.waiters {
		if !w.at.After(f.now) {
			w.ch <- f.now
			continue
		}
		f.waiters[n] = w
		n++
	}
	clear(f.waiters[n:])
	f.waiters = f.waiters[:n]
}

// Waiters returns the number of timers not yet fired.
func (f *Fake) Waiters() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.waiters)
}

// WaitForWaiters blocks until at least n timers are pending or timeout passes
// in real time. It reports whether the count was reached. Tests use it to know
// that a goroutine is parked on the clock before advancing it.
func (f *Fake) WaitForWaiters(n int, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for {
		if f.Waiters() >= n {
			return true
		}
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(time.Millisecond)
	}
}
