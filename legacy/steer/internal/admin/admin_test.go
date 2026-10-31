package admin

import (
	"encoding/json"
	"errors"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/laiwf94/steer/internal/metrics"
)

type fixture struct {
	h       *Handler
	ready   atomic.Bool
	reloads atomic.Int32
	failNow atomic.Bool
}

func newFixture(t *testing.T, token string) *fixture {
	t.Helper()
	f := &fixture{}
	f.ready.Store(true)
	m := metrics.New()
	m.Counter("steer_test_total", "", "Test.").Inc()
	h, err := New(Config{
		Token:   token,
		Metrics: m,
		State:   func() any { return map[string]any{"clusters": []string{"web"}} },
		Ready:   f.ready.Load,
		Reload: func() (uint64, error) {
			if f.failNow.Load() {
				return 0, errors.New("invalid configuration:\nclusters[0].name: duplicate")
			}
			return uint64(f.reloads.Add(1)) + 1, nil
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	f.h = h
	return f
}

func (f *fixture) call(method, path, token string) (*httptest.ResponseRecorder, map[string]any) {
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(method, path, nil)
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	f.h.ServeHTTP(rec, req)
	var out map[string]any
	_ = json.Unmarshal(rec.Body.Bytes(), &out)
	return rec, out
}

func TestOpenEndpoints(t *testing.T) {
	f := newFixture(t, "admin-token-0123456789")
	if rec, out := f.call("GET", "/healthz", ""); rec.Code != 200 || out["status"] != "ok" {
		t.Fatalf("healthz %d %v", rec.Code, out)
	}
	if rec, out := f.call("GET", "/readyz", ""); rec.Code != 200 || out["status"] != "ready" {
		t.Fatalf("readyz %d %v", rec.Code, out)
	}
	f.ready.Store(false)
	if rec, out := f.call("GET", "/readyz", ""); rec.Code != 503 || out["status"] != "draining" {
		t.Fatalf("draining readyz %d %v", rec.Code, out)
	}
}

func TestAuth(t *testing.T) {
	const tok = "admin-token-0123456789"
	f := newFixture(t, tok)
	for _, path := range []string{"/metrics", "/clusters"} {
		for _, bad := range []string{"", "wrong-token-0123456789", tok + "x", tok[:len(tok)-1]} {
			rec, out := f.call("GET", path, bad)
			e, _ := out["error"].(map[string]any)
			if rec.Code != 401 || e["code"] != "unauthenticated" || e["request_id"] == "" || rec.Header().Get("WWW-Authenticate") == "" {
				t.Fatalf("%s with %q: %d %v", path, bad, rec.Code, out)
			}
		}
	}
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("GET", "/clusters", nil)
	req.Header.Set("Authorization", "Basic "+tok)
	f.h.ServeHTTP(rec, req)
	if rec.Code != 401 {
		t.Fatalf("Basic scheme accepted: %d", rec.Code)
	}
	if rec, _ := f.call("POST", "/reload", ""); rec.Code != 401 || f.reloads.Load() != 0 {
		t.Fatal("unauthenticated reload ran")
	}
	if rec, out := f.call("GET", "/clusters", tok); rec.Code != 200 || out["clusters"] == nil {
		t.Fatalf("clusters %d %v", rec.Code, out)
	}
	rec, _ = f.call("GET", "/metrics", tok)
	if rec.Code != 200 || !strings.Contains(rec.Body.String(), "steer_test_total 1") || !strings.HasPrefix(rec.Header().Get("Content-Type"), "text/plain") {
		t.Fatalf("metrics %d %s", rec.Code, rec.Body)
	}
}

func TestNoTokenMeansOpen(t *testing.T) {
	f := newFixture(t, "")
	if rec, _ := f.call("GET", "/clusters", ""); rec.Code != 200 {
		t.Fatalf("loopback mode: %d", rec.Code)
	}
}

func TestReload(t *testing.T) {
	const tok = "admin-token-0123456789"
	f := newFixture(t, tok)
	rec, out := f.call("POST", "/reload", tok)
	if rec.Code != 200 || out["status"] != "reloaded" || out["config_version"] != float64(2) {
		t.Fatalf("%d %v", rec.Code, out)
	}
	f.failNow.Store(true)
	rec, out = f.call("POST", "/reload", tok)
	e, _ := out["error"].(map[string]any)
	if rec.Code != 400 || e["code"] != "invalid_config" || !strings.Contains(e["message"].(string), "duplicate") {
		t.Fatalf("%d %v", rec.Code, out)
	}
	if rec, _ = f.call("GET", "/reload", tok); rec.Code != 405 || rec.Header().Get("Allow") != "POST" {
		t.Fatalf("GET /reload: %d", rec.Code)
	}
}

func TestNotFoundAndHeaders(t *testing.T) {
	f := newFixture(t, "")
	rec, out := f.call("GET", "/nope", "")
	e, _ := out["error"].(map[string]any)
	if rec.Code != 404 || e["code"] != "not_found" || rec.Header().Get("X-Request-Id") == "" || e["request_id"] != rec.Header().Get("X-Request-Id") {
		t.Fatalf("%d %v %v", rec.Code, out, rec.Header())
	}
	rec, _ = f.call("GET", "/healthz", "")
	if rec.Header().Get("Cache-Control") != "no-store" || rec.Header().Get("X-Content-Type-Options") != "nosniff" {
		t.Fatalf("headers %v", rec.Header())
	}
}

func TestNewValidates(t *testing.T) {
	if _, err := New(Config{}); err == nil {
		t.Fatal("missing callbacks accepted")
	}
	f := newFixture(t, "")
	cfg := f.h.cfg
	cfg.Token = "short"
	if _, err := New(cfg); err == nil {
		t.Fatal("short token accepted")
	}
}

func TestMetricsWriteErrorIsLogged(t *testing.T) {
	f := newFixture(t, "")
	f.h.metrics(&statusWriter{ResponseWriter: errWriter{httptest.NewRecorder()}}, httptest.NewRequest("GET", "/metrics", nil))
}

type errWriter struct{ *httptest.ResponseRecorder }

func (errWriter) Write([]byte) (int, error) { return 0, errors.New("broken pipe") }
