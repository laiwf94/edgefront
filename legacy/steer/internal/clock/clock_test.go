package clock

import (
	"testing"
	"time"
)

var epoch = time.Unix(1_000_000, 0)

func TestFakeFiresInOrder(t *testing.T) {
	f := NewFake(epoch)
	late := f.After(2 * time.Second)
	early := f.After(time.Second)
	if f.Waiters() != 2 {
		t.Fatalf("waiters = %d", f.Waiters())
	}
	f.Advance(500 * time.Millisecond)
	select {
	case <-early:
		t.Fatal("fired early")
	default:
	}
	f.Advance(500 * time.Millisecond)
	if got := <-early; !got.Equal(epoch.Add(time.Second)) {
		t.Fatalf("early fired at %v", got)
	}
	select {
	case <-late:
		t.Fatal("late fired too soon")
	default:
	}
	f.Advance(time.Hour)
	<-late
	if f.Waiters() != 0 || !f.Now().Equal(epoch.Add(time.Hour+time.Second)) {
		t.Fatalf("waiters %d now %v", f.Waiters(), f.Now())
	}
}

func TestFakeZeroDurationFiresImmediately(t *testing.T) {
	f := NewFake(epoch)
	select {
	case <-f.After(0):
	default:
		t.Fatal("zero duration did not fire")
	}
}

func TestWaitForWaiters(t *testing.T) {
	f := NewFake(epoch)
	if f.WaitForWaiters(1, 5*time.Millisecond) {
		t.Fatal("no waiter yet")
	}
	go f.After(time.Second)
	if !f.WaitForWaiters(1, 5*time.Second) {
		t.Fatal("waiter not seen")
	}
}

func TestReal(t *testing.T) {
	var c Clock = Real{}
	before := time.Now()
	<-c.After(time.Millisecond)
	if c.Now().Before(before) {
		t.Fatal("real clock went backwards")
	}
}
