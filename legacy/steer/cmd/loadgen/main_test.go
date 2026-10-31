package main

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func TestRunCountsOutcomes(t *testing.T) {
	var n atomic.Int64
	var gotHeader atomic.Value
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotHeader.Store(r.Header.Get("X-User"))
		switch n.Add(1) % 10 {
		case 0:
			w.WriteHeader(503)
		case 5:
			time.Sleep(300 * time.Millisecond) // beyond the timeout
		}
	}))
	defer srv.Close()
	var out bytes.Buffer
	err := run(context.Background(), []string{"-url", srv.URL, "-rate", "200", "-duration", "1s", "-timeout", "100ms",
		"-H", "x-user: alice", "-format", "json"}, &out, io.Discard)
	if err != nil {
		t.Fatal(err)
	}
	var r Report
	if err := json.Unmarshal(out.Bytes(), &r); err != nil {
		t.Fatal(err)
	}
	if r.Requests != 200 || r.Failures["http_503"] != 20 || r.Failures["timeout"] != 20 || r.OK != 160 || r.Success != 80 {
		t.Fatalf("report %+v", r)
	}
	if r.P999Ms < 100 || r.P50Ms > 100 || r.MaxMs < r.P999Ms {
		t.Fatalf("latency %+v", r)
	}
	if gotHeader.Load() != "alice" {
		t.Fatalf("header %v", gotHeader.Load())
	}
}

func TestOpenLoopKeepsSchedule(t *testing.T) {
	// Every response takes 200ms, yet 50 requests at 100/s still go out in
	// about half a second: the generator does not wait for answers.
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { time.Sleep(200 * time.Millisecond) }))
	defer srv.Close()
	var out bytes.Buffer
	start := time.Now()
	if err := run(context.Background(), []string{"-url", srv.URL, "-rate", "100", "-duration", "500ms", "-format", "row", "-label", "slow"}, &out, io.Discard); err != nil {
		t.Fatal(err)
	}
	if el := time.Since(start); el > 2*time.Second {
		t.Fatalf("took %v: the generator waited for responses", el)
	}
	if !strings.HasPrefix(out.String(), "| slow | 50 | 100.00% |") {
		t.Fatalf("row %q", out.String())
	}
}

func TestDroppedWhenSaturated(t *testing.T) {
	release := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { <-release }))
	defer srv.Close()
	defer close(release)
	var out bytes.Buffer
	go func() { time.Sleep(400 * time.Millisecond); release <- struct{}{}; release <- struct{}{} }()
	err := run(context.Background(), []string{"-url", srv.URL, "-rate", "100", "-duration", "200ms", "-max-inflight", "2",
		"-timeout", "1s"}, &out, io.Discard)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out.String(), "dropped x18") || !strings.Contains(out.String(), "requests 20") {
		t.Fatalf("output %q", out.String())
	}
}

func TestCancelStopsScheduling(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	defer srv.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	var out bytes.Buffer
	start := time.Now()
	if err := run(ctx, []string{"-url", srv.URL, "-rate", "10", "-duration", "1h"}, &out, io.Discard); err != nil {
		t.Fatal(err)
	}
	if time.Since(start) > 5*time.Second || !strings.Contains(out.String(), "failures: none") {
		t.Fatalf("output %q after %v", out.String(), time.Since(start))
	}
}

func TestTransportErrorAndBadMethod(t *testing.T) {
	var out bytes.Buffer
	if err := run(context.Background(), []string{"-url", "http://127.0.0.1:1/", "-rate", "10", "-duration", "100ms"}, &out, io.Discard); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(out.String(), "transport_error") {
		t.Fatalf("output %q", out.String())
	}
	if s := send(http.DefaultClient, options{method: "GET", url: "http://a b/"}, time.Now()); s.outcome != "bad_request" {
		t.Fatalf("outcome %q", s.outcome)
	}
}

func TestFlagErrors(t *testing.T) {
	cases := map[string][]string{
		"-url must be":          {},
		"-rate must be":         {"-url", "http://x", "-rate", "0"},
		"-duration must be":     {"-url", "http://x", "-duration", "0s"},
		"-timeout must be":      {"-url", "http://x", "-timeout", "0s"},
		"-max-inflight must be": {"-url", "http://x", "-max-inflight", "0"},
		"-format must be":       {"-url", "http://x", "-format", "xml"},
		"-method must be":       {"-url", "http://x", "-method", "get"},
		"unexpected arguments":  {"-url", "http://x", "extra"},
		"must look like":        {"-url", "http://x", "-H", "novalue"},
		"not a valid token":     {"-url", "http://x", "-H", "bad name: v"},
		"control characters":    {"-url", "http://x", "-H", "X-A: v\x01"},
		"flag provided but not": {"-nope"},
		"-url must be an http":  {"-url", "ftp://x"},
	}
	for want, args := range cases {
		err := run(context.Background(), args, io.Discard, io.Discard)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%v: err %v, want %q", args, err, want)
		}
	}
	var h headerFlag
	h.Set("A: 1")
	if h.String() == "" {
		t.Fatal("String")
	}
}

func TestPercentile(t *testing.T) {
	if percentile(nil, 0.5) != 0 {
		t.Fatal("empty")
	}
	var ds []time.Duration
	for i := 1; i <= 1000; i++ {
		ds = append(ds, time.Duration(i)*time.Millisecond)
	}
	if percentile(ds, 0.5) != 500 || percentile(ds, 0.99) != 990 || percentile(ds, 0.999) != 999 || percentile(ds, 0) != 1 {
		t.Fatalf("p50 %v p99 %v p999 %v", percentile(ds, 0.5), percentile(ds, 0.99), percentile(ds, 0.999))
	}
	if r := summarise(nil, time.Second); r.Success != 0 || r.MaxMs != 0 {
		t.Fatalf("empty summary %+v", r)
	}
}

func FuzzParseHeader(f *testing.F) {
	f.Add("X-User: alice")
	f.Add(": v")
	f.Add("A:\x00")
	f.Fuzz(func(t *testing.T, s string) {
		name, value, err := parseHeader(s)
		if err != nil {
			return
		}
		h := http.Header{}
		h.Set(name, value)
		var b bytes.Buffer
		if err := h.Write(&b); err != nil {
			t.Fatal(err)
		}
		if strings.Count(b.String(), "\r\n") != 1 {
			t.Fatalf("%q produced a header that splits into %q", s, b.String())
		}
	})
}
