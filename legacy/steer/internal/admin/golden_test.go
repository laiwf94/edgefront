package admin

// The admin API's answers, recorded for edgefront's Rust port: every case
// below runs through this handler, and the result must match the golden
// file that crates/edgefront-core/tests/admin_steer.rs replays against the
// Rust API. `go test ./internal/admin -run Golden -update` rewrites it.

import (
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"math"
	"net/http/httptest"
	"os"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/balancer"
	"github.com/laiwf94/steer/internal/endpoint"
	"github.com/laiwf94/steer/internal/metrics"
	"github.com/laiwf94/steer/internal/proxy"
	"github.com/laiwf94/steer/internal/reqid"
)

var update = flag.Bool("update", false, "rewrite the admin golden file")

const goldenPath = "../../../../crates/edgefront-core/tests/testdata/admin_steer.json"

// goldenToken is the token of every case with "token": true.
const goldenToken = "admin-token-0123456789"

// goldenCase is one request and steer's answer. A request ID steer made
// up is written as "<generated>" in the header and the body.
type goldenCase struct {
	Name        string            `json:"name"`
	Token       bool              `json:"token"`
	State       string            `json:"state"` // "full", "empty" or "nan"
	Draining    bool              `json:"draining,omitempty"`
	ReloadError string            `json:"reload_error,omitempty"`
	Method      string            `json:"method"`
	Target      string            `json:"target"`
	Header      map[string]string `json:"header,omitempty"`

	Status         int               `json:"status"`
	ResponseHeader map[string]string `json:"response_header"`
	Body           string            `json:"body"`
}

func goldenState(kind string) proxy.State {
	loaded := time.Date(2026, 11, 15, 1, 2, 3, 450000000, time.UTC)
	switch kind {
	case "empty":
		return proxy.State{ConfigVersion: 1, LoadedAt: loaded}
	case "nan":
		return proxy.State{ConfigVersion: 3, LoadedAt: loaded, Clusters: []proxy.ClusterState{{
			Name: "web", Policy: "round_robin", MaxRequests: 1,
			Balancer: balancer.State{Healthy: []int{1}, Total: []int{1}, PriorityLoad: []int{100}},
			Endpoints: []proxy.EndpointState{{Address: "10.0.0.1:80", Weight: 1,
				Snapshot: endpoint.Snapshot{Healthy: true, LatencyMs: math.NaN(), Available: true}}},
		}}}
	}
	balance := 12.5
	return proxy.State{ConfigVersion: 7, LoadedAt: loaded, Clusters: []proxy.ClusterState{
		{
			Name: "web", Policy: "least_loaded", InFlight: 3, MaxRequests: 1024, RetryBudgetBalance: &balance,
			Balancer: balancer.State{Healthy: []int{1, 1}, Total: []int{2, 1}, PriorityLoad: []int{50, 50}},
			Endpoints: []proxy.EndpointState{
				{Address: "10.0.0.1:8080", Weight: 3, Priority: 0, Snapshot: endpoint.Snapshot{
					Healthy: true, Ejected: false, TimesEjected: 0, InFlight: 2, LatencyMs: 1.234, Available: true}},
				{Address: "10.0.0.2:8080", Weight: 1, Priority: 0, Snapshot: endpoint.Snapshot{
					Healthy: true, Ejected: true, EjectedUntil: time.Date(2026, 11, 15, 1, 2, 33, 450000001, time.UTC),
					TimesEjected: 2, LastEjectReason: "consecutive_gateway_failure", ConsecutiveFailures: 5,
					InFlight: 0, LatencyMs: 0.0000005, Available: false}},
				{Address: "[2001:db8::1]:8080", Weight: 1, Priority: 1, Snapshot: endpoint.Snapshot{
					Healthy: false, LastProbeError: `GET /healthz: status 503 <"down" & out>`, InFlight: 1,
					LatencyMs: 250, Available: false}},
			},
		},
		{
			Name: "api", Policy: "maglev", InFlight: 0, MaxRequests: 1,
			Balancer: balancer.State{Healthy: []int{0}, Total: []int{1}, PriorityLoad: []int{0}, Panic: true},
			Endpoints: []proxy.EndpointState{
				{Address: "api.internal:443", Weight: 100, Priority: 0, Snapshot: endpoint.Snapshot{
					Healthy: false, Ejected: true, EjectedUntil: time.Date(2026, 11, 15, 1, 3, 0, 0, time.UTC),
					TimesEjected: 1, LastEjectReason: "success_rate", LatencyMs: 1e21}},
			},
		},
	}}
}

func goldenCases() []goldenCase {
	auth := map[string]string{"Authorization": "Bearer " + goldenToken}
	tok := func(name, method, target string, header map[string]string) goldenCase {
		return goldenCase{Name: name, Token: true, State: "full", Method: method, Target: target, Header: header}
	}
	open := func(name, method, target string) goldenCase {
		return goldenCase{Name: name, State: "full", Method: method, Target: target}
	}
	draining := tok("readyz draining", "GET", "/readyz", nil)
	draining.Draining = true
	failed := tok("reload refused", "POST", "/reload", auth)
	failed.ReloadError = "invalid configuration:\nclusters[0].name: duplicate <web>"
	empty := open("clusters with none", "GET", "/clusters")
	empty.State = "empty"
	nan := open("clusters with a NaN", "GET", "/clusters")
	nan.State = "nan"
	cases := []goldenCase{
		tok("healthz", "GET", "/healthz", nil),
		tok("readyz", "GET", "/readyz", nil),
		draining,
		tok("healthz by POST", "POST", "/healthz", nil),
		tok("readyz by HEAD", "HEAD", "/readyz", nil),
		tok("healthz with the token", "GET", "/healthz", auth),
		tok("metrics without a token", "GET", "/metrics", nil),
		tok("metrics with a wrong token", "GET", "/metrics", map[string]string{"Authorization": "Bearer wrong-token-0123456789"}),
		tok("metrics with a longer token", "GET", "/metrics", map[string]string{"Authorization": "Bearer " + goldenToken + "x"}),
		tok("metrics with a prefix of the token", "GET", "/metrics", map[string]string{"Authorization": "Bearer " + goldenToken[:len(goldenToken)-1]}),
		tok("metrics by Basic", "GET", "/metrics", map[string]string{"Authorization": "Basic " + goldenToken}),
		tok("metrics with an empty bearer", "GET", "/metrics", map[string]string{"Authorization": "Bearer "}),
		tok("metrics with a lower-case scheme", "GET", "/metrics", map[string]string{"Authorization": "bearer " + goldenToken}),
		tok("metrics with two spaces", "GET", "/metrics", map[string]string{"Authorization": "Bearer  " + goldenToken}),
		tok("metrics", "GET", "/metrics", auth),
		tok("clusters", "GET", "/clusters", auth),
		tok("clusters without a token", "GET", "/clusters", nil),
		tok("clusters by POST without a token", "POST", "/clusters", nil),
		tok("clusters by DELETE", "DELETE", "/clusters", auth),
		tok("reload", "POST", "/reload", auth),
		failed,
		tok("reload by GET", "GET", "/reload", auth),
		tok("reload by GET without a token", "GET", "/reload", nil),
		tok("reload without a token", "POST", "/reload", nil),
		open("clusters open", "GET", "/clusters"),
		open("reload open", "POST", "/reload"),
		open("metrics open", "GET", "/metrics"),
		empty,
		nan,
		tok("unknown path", "GET", "/nope", nil),
		tok("unknown path with the token", "POST", "/nope", auth),
		tok("trailing slash", "GET", "/healthz/", nil),
		tok("root", "GET", "/", nil),
		tok("double slash", "GET", "//healthz", nil),
		tok("dot dot with a query", "GET", "/a/../healthz?x=1&y='2'%3C", nil),
		tok("dot", "GET", "/./readyz", nil),
		tok("dot dot by POST", "POST", "/x/../reload", auth),
		tok("dot dot by HEAD", "HEAD", "/x/..//clusters", nil),
		tok("escaped letter", "GET", "/heal%74hz", nil),
		tok("escaped dots", "GET", "/%2E%2E/healthz", nil),
		tok("escaped specials", "GET", "/q/./%3Cb%3E&x%20y!", nil),
		tok("unclean unknown", "GET", "/a//b/", nil),
		tok("kept request id", "GET", "/healthz", map[string]string{"X-Request-Id": "abc-123.x_y:z"}),
		tok("kept request id on an error", "GET", "/metrics", map[string]string{"X-Request-Id": "trace:1.2-3_4"}),
		tok("replaced request id", "GET", "/healthz", map[string]string{"X-Request-Id": "has space"}),
		tok("request id on not found", "GET", "/nope", map[string]string{"X-Request-Id": "abc-123"}),
	}
	return cases
}

func runGolden(t *testing.T, c goldenCase) goldenCase {
	t.Helper()
	m := metrics.New()
	m.Counter("steer_test_total", "", "Test.").Inc()
	token := ""
	if c.Token {
		token = goldenToken
	}
	h, err := New(Config{
		Token:   token,
		Metrics: m,
		State:   func() any { return goldenState(c.State) },
		Ready:   func() bool { return !c.Draining },
		Reload: func() (uint64, error) {
			if c.ReloadError != "" {
				return 0, errors.New(c.ReloadError)
			}
			return 8, nil
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(c.Method, c.Target, nil)
	for k, v := range c.Header {
		req.Header.Set(k, v)
	}
	h.ServeHTTP(rec, req)
	out := c
	out.Status = rec.Code
	out.Body = rec.Body.String()
	out.ResponseHeader = map[string]string{}
	keys := make([]string, 0, len(rec.Header()))
	for k := range rec.Header() {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		out.ResponseHeader[strings.ToLower(k)] = strings.Join(rec.Header().Values(k), ", ")
	}
	if id := rec.Header().Get(reqid.Header); id != "" && id != c.Header["X-Request-Id"] {
		if len(id) != 24 || !reqid.Valid(id) {
			t.Fatalf("%s: generated request ID %q", c.Name, id)
		}
		out.ResponseHeader["x-request-id"] = "<generated>"
		out.Body = strings.ReplaceAll(out.Body, id, "<generated>")
	}
	return out
}

func TestGolden(t *testing.T) {
	var got []goldenCase
	for _, c := range goldenCases() {
		got = append(got, runGolden(t, c))
	}
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetIndent("", "  ")
	enc.SetEscapeHTML(false)
	if err := enc.Encode(got); err != nil {
		t.Fatal(err)
	}
	if *update {
		if err := os.WriteFile(goldenPath, buf.Bytes(), 0o644); err != nil {
			t.Fatal(err)
		}
		return
	}
	want, err := os.ReadFile(goldenPath)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(want, buf.Bytes()) {
		t.Fatalf("%s is out of date; rerun with -update and review the diff", goldenPath)
	}
}
