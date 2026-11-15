package main

import (
	"bytes"
	"encoding/json"
	"slices"
	"strings"
	"testing"
)

func resilienceQuery(t *testing.T, q query) string {
	t.Helper()
	b, err := json.Marshal(q)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

// The worked example that edgefront-difftest's resilience tests expect the
// Rust detector to answer in the same words.
func TestOutlierWorkedExample(t *testing.T) {
	script := strings.Join([]string{
		"f 0 1 1", "f 0 1 1", "f 0 1 1", "s",
		"f 1 0 1", "f 1 0 1", "f 1 0 1", "f 1 0 1",
		"f 2 0 0", "f 2 0 0", "f 2 0 0", "f 2 0 0",
		"f 3 0 0", "f 3 0 0", "f 3 0 0", "f 3 0 0", "k",
		"f 2 1 1", "f 2 1 1", "f 2 1 1",
		"t 10000000000", "k", "f 2 1 1",
		"f 0 1 1", "f 0 1 1", "f 0 1 1",
		"t 45000000000", "k", "s", "c 2", "s",
	}, "\n")
	as := ask(t, resilienceQuery(t, query{Op: "outlier", Hosts: 4, Script: script, Outlier: &outlierSpec{
		ConsecutiveGatewayFailures: 3, Interval: 1e9, BaseEjectionTime: 10e9, MaxEjectionTime: 25e9,
		MaxEjectionPercent: 50, SuccessRateMinimumHosts: 3, SuccessRateRequestVolume: 4, SuccessRateStdevFactor: 1,
	}}))
	ok := func(n int) []string { return slices.Repeat([]string{"ok"}, n) }
	want := slices.Concat(
		ok(2),
		[]string{"eject 0 consecutive_gateway_failure 10000000000", "state 1/10000000000 0/- 0/- 0/-"},
		ok(12),
		// Rates 0, 1 and 1 among the three un-ejected hosts.
		[]string{"eject 1 success_rate 10000000000"},
		// Two of four ejected: the cap holds endpoint 2 back.
		ok(3),
		// Both ejections end; endpoint 2's run of failures carries on.
		ok(2),
		[]string{"eject 2 consecutive_gateway_failure 20000000000"},
		ok(2),
		// Ejected a second time: twice the base.
		[]string{"eject 0 consecutive_gateway_failure 30000000000"},
		// 25 s un-ejected decay endpoint 0's multiplier from 2 to 0.
		ok(2),
		[]string{"state 0/30000000000 0/10000000000 0/20000000000 0/-", "ok", "state 0/30000000000 0/10000000000 0/- 0/-"},
	)
	if len(as) != 1 || !as[0].OK || !slices.Equal(as[0].Lines, want) {
		t.Fatalf("got %+v\nwant %q", as, want)
	}
}

func TestRetryWorkedExample(t *testing.T) {
	spec := &retrySpec{MaxRetries: 2, RetryOn: []int{502, 503}, BackoffBase: 25e6, BackoffMax: 40e6,
		Ratio: 0.5, MinPerSecond: 1, Window: 2e9}
	script := strings.Join([]string{
		"q GET 0 1 503 c", "q get 0 1 503", "q POST 0 1 503", "q POST 1 1 504", "q POST 0 1 c t",
		"q PUT 0 0 c", "q DELETE 0 1 r t s", "b", "t 2000000000", "b", "q HEAD 0 1 c c c c", "b",
	}, "\n")
	as := ask(t,
		resilienceQuery(t, query{Op: "retry", Hosts: 3, Seed: 9, Retry: spec, Script: script}),
		resilienceQuery(t, query{Op: "retry", Hosts: 1, Retry: spec, Script: "q GET 0 1 c\nq OPTIONS 0 1 500 599 s"}),
		resilienceQuery(t, query{Op: "retry", Hosts: 2, Retry: &retrySpec{MaxRetries: 5, RetryOn: []int{}, BackoffBase: 1e6,
			BackoffMax: 1e6, Window: 1e9}, Script: "q TRACE 0 1 t 503"}),
	)
	want := [][]string{{
		"g r17059068,g r30027795,o d", // a connect error is retried for any method
		"g d",                         // methods are case-sensitive: get is not GET
		"g d",                         // POST is not idempotent
		"g d",                         // an Idempotency-Key makes it safe, but 504 is not in retry_on
		"g r6633061,g d",              // a timeout is retried only when safe
		"g d",                         // a body that cannot be replayed
		"g r19620342,g r10502136,o d", // a reset, a timeout, then success
		"balance 0.5",                 // 2 + 0.5 x 7 requests - 5 retries
		"ok",
		"balance 2",                  // the window has passed
		"g r2865202,g r25832349,g d", // max_retries reached
		"balance 0.5",
	}, {
		"g n", // no other endpoint to retry on
		"f d", // 500 is not a gateway failure, and not in retry_on
	}, {
		"g x", // a zero budget refuses every retry
	}}
	if len(as) != 3 {
		t.Fatalf("got %d answers", len(as))
	}
	for i, a := range as {
		if !a.OK || !slices.Equal(a.Lines, want[i]) {
			t.Errorf("query %d: got %q\nwant %q", i, a.Lines, want[i])
		}
	}
}

func TestMalformedResilienceQueriesStop(t *testing.T) {
	od := &outlierSpec{Interval: 1e9, BaseEjectionTime: 1e9, MaxEjectionTime: 1e9}
	rs := &retrySpec{Window: 1e9}
	for _, q := range []query{
		{Op: "outlier", Hosts: 0, Outlier: od, Script: "k"},
		{Op: "outlier", Hosts: 1, Script: "k"},
		{Op: "outlier", Hosts: 1, Outlier: &outlierSpec{BaseEjectionTime: 2e9, MaxEjectionTime: 1e9}, Script: "k"},
		{Op: "outlier", Hosts: 1, Outlier: od, Script: "f 1 0 0"},
		{Op: "outlier", Hosts: 1, Outlier: od, Script: "f 0 2 0"},
		{Op: "outlier", Hosts: 1, Outlier: od, Script: "t -1"},
		{Op: "outlier", Hosts: 1, Outlier: od, Script: ""},
		{Op: "retry", Hosts: 1, Retry: &retrySpec{Window: 1}, Script: "b"},
		{Op: "retry", Hosts: 1, Retry: &retrySpec{Window: 1e9, RetryOn: []int{404}}, Script: "b"},
		{Op: "retry", Hosts: 1, Script: "b"},
		{Op: "retry", Hosts: 1, Retry: rs, Script: "q GET 0 1 404"},
		{Op: "retry", Hosts: 1, Retry: rs, Script: "q GET 2 1"},
		{Op: "retry", Hosts: 1, Retry: rs, Script: "t x"},
		{Op: "retry", Hosts: 1, Retry: rs, Script: "z"},
	} {
		var out bytes.Buffer
		if err := serve(strings.NewReader(resilienceQuery(t, q)), &out); err == nil {
			t.Errorf("%+v: served %q", q, out.String())
		}
	}
}
