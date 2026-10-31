package health

import (
	"context"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
)

func cfg() config.HealthCheck {
	return config.HealthCheck{Path: "/healthz", Interval: time.Second, Jitter: 100 * time.Millisecond,
		Timeout: 200 * time.Millisecond, HealthyThreshold: 2, UnhealthyThreshold: 3}
}

type changes struct {
	mu  sync.Mutex
	got []string
}

func (c *changes) record(ep *endpoint.Endpoint, healthy bool, reason string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	s := "down"
	if healthy {
		s = "up"
	}
	c.got = append(c.got, s+":"+reason)
}

func (c *changes) list() []string {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]string(nil), c.got...)
}

func TestThresholds(t *testing.T) {
	var status atomic.Int32
	status.Store(200)
	var host atomic.Value
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		host.Store(r.Host)
		if r.URL.Path != "/healthz" || r.Header.Get("User-Agent") != "steer-health-check" {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		w.WriteHeader(int(status.Load()))
	}))
	defer srv.Close()
	ep := endpoint.New(srv.Listener.Addr().String(), 1, 0)
	var ch changes
	c := New(cfg(), []*endpoint.Endpoint{ep}, nil, ch.record)
	ctx := context.Background()

	c.CheckOnce(ctx, ep)
	if !ep.Healthy() || len(ch.list()) != 0 {
		t.Fatal("healthy endpoint changed state")
	}
	status.Store(503)
	c.CheckOnce(ctx, ep)
	c.CheckOnce(ctx, ep)
	if !ep.Healthy() {
		t.Fatal("unhealthy before the threshold")
	}
	c.CheckOnce(ctx, ep)
	if ep.Healthy() {
		t.Fatal("still healthy after 3 failures")
	}
	if got := ch.list(); len(got) != 1 || got[0] != "down:probe returned status 503" {
		t.Fatalf("changes %v", got)
	}
	if s := ep.Snapshot(time.Now()); s.LastProbeError == "" || s.Available {
		t.Fatalf("snapshot %+v", s)
	}
	status.Store(204)
	c.CheckOnce(ctx, ep)
	if ep.Healthy() {
		t.Fatal("healthy after one pass")
	}
	c.CheckOnce(ctx, ep)
	if !ep.Healthy() {
		t.Fatal("not healthy after 2 passes")
	}
	if host.Load() != ep.Address {
		t.Fatalf("Host %v", host.Load())
	}
}

func TestProbeFailures(t *testing.T) {
	// Redirects are failures, not followed.
	redirect := httptest.NewServer(http.RedirectHandler("/elsewhere", http.StatusFound))
	defer redirect.Close()
	// A server that never answers: the timeout must fire.
	hang := make(chan struct{})
	slow := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-hang:
		case <-r.Context().Done():
		}
	}))
	defer slow.Close()
	defer close(hang)
	// A closed port refuses connections.
	ln, _ := net.Listen("tcp", "127.0.0.1:0")
	closed := ln.Addr().String()
	ln.Close()

	c := New(cfg(), nil, nil, nil)
	cases := map[string]string{
		redirect.Listener.Addr().String(): "status 302",
		slow.Listener.Addr().String():     "probe failed",
		closed:                            "refused",
	}
	for addr, want := range cases {
		ep := endpoint.New(addr, 1, 0)
		start := time.Now()
		err := c.probe(context.Background(), ep)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%s: err %v, want %q", addr, err, want)
		}
		if time.Since(start) > 2*time.Second {
			t.Errorf("%s: probe took %v", addr, time.Since(start))
		}
	}
	bad := endpoint.New("bad host:1", 1, 0)
	if err := c.probe(context.Background(), bad); err == nil {
		t.Fatal("invalid URL accepted")
	}
}

func TestCustomHostHeader(t *testing.T) {
	var host atomic.Value
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { host.Store(r.Host) }))
	defer srv.Close()
	hc := cfg()
	hc.Host = "health.internal"
	c := New(hc, nil, nil, nil)
	if err := c.probe(context.Background(), endpoint.New(srv.Listener.Addr().String(), 1, 0)); err != nil {
		t.Fatal(err)
	}
	if host.Load() != "health.internal" {
		t.Fatalf("host %v", host.Load())
	}
}

// Run probes on the injected clock: nothing happens between ticks, and every
// tick probes again.
func TestRunFollowsTheClock(t *testing.T) {
	var probes atomic.Int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		probes.Add(1)
		w.WriteHeader(http.StatusServiceUnavailable)
	}))
	defer srv.Close()
	clk := clock.NewFake(time.Unix(0, 0))
	eps := []*endpoint.Endpoint{endpoint.New(srv.Listener.Addr().String(), 1, 0)}
	var ch changes
	c := New(cfg(), eps, clk, ch.record)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() { c.Run(ctx); close(done) }()
	for want := int32(1); want <= 3; want++ {
		if !clk.WaitForWaiters(1, 5*time.Second) {
			t.Fatal("probe loop is not waiting on the clock")
		}
		if got := probes.Load(); got != want {
			t.Fatalf("probes %d want %d", got, want)
		}
		clk.Advance(cfg().Interval + cfg().Jitter)
	}
	if !clk.WaitForWaiters(1, 5*time.Second) {
		t.Fatal("probe loop stopped")
	}
	if eps[0].Healthy() {
		t.Fatal("still healthy after 3 failed probes")
	}
	cancel()
	<-done
	if got := ch.list(); len(got) != 1 {
		t.Fatalf("changes %v", got)
	}
}

// A probe canceled by shutdown must not count as a failure.
func TestCanceledProbeIsIgnored(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { <-r.Context().Done() }))
	defer srv.Close()
	ep := endpoint.New(srv.Listener.Addr().String(), 1, 0)
	hc := cfg()
	hc.UnhealthyThreshold = 1
	c := New(hc, []*endpoint.Endpoint{ep}, nil, nil)
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	c.CheckOnce(ctx, ep)
	if !ep.Healthy() {
		t.Fatal("canceled probe marked the endpoint down")
	}
}
