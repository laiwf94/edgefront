package fakeupstream

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestParseSettings(t *testing.T) {
	cur := Settings{Latency: time.Second}
	got, err := ParseSettings(cur, []byte(`{"error_rate":0.5,"blackhole":true}`))
	if err != nil || got.ErrorRate != 0.5 || !got.Blackhole || got.Latency != time.Second {
		t.Fatalf("%+v %v", got, err)
	}
	got, err = ParseSettings(got, []byte(`{"latency":"0s","blackhole":false}`))
	if err != nil || got.Latency != 0 || got.Blackhole || got.ErrorRate != 0.5 {
		t.Fatalf("%+v %v", got, err)
	}
	for _, bad := range []string{`nope`, `{"x":1}`, `{} {}`, `{"latency":"-1s"}`, `{"latency":"2m"}`, `{"latency":"soon"}`,
		`{"error_rate":1.5}`, `{"error_rate":-0.1}`, `{"blackhole":"yes"}`, strings.Repeat(" ", MaxControlBody+1)} {
		if _, err := ParseSettings(cur, []byte(bad)); err == nil {
			t.Errorf("accepted %q", bad)
		}
	}
}

func FuzzParseSettings(f *testing.F) {
	f.Add([]byte(`{"latency":"10ms","error_rate":0.25,"blackhole":false}`))
	f.Add([]byte(`{}`))
	f.Fuzz(func(t *testing.T, data []byte) {
		s, err := ParseSettings(Settings{}, data)
		if err != nil {
			return
		}
		if s.Latency < 0 || s.Latency > MaxLatency || s.ErrorRate < 0 || s.ErrorRate > 1 {
			t.Fatalf("out of range %+v", s)
		}
	})
}

func get(t *testing.T, h http.Handler, path string) *httptest.ResponseRecorder {
	t.Helper()
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, path, nil))
	return rec
}

func TestFaults(t *testing.T) {
	s := New("a")
	h := s.Handler()
	if rec := get(t, h, "/x"); rec.Code != 200 || !strings.Contains(rec.Body.String(), `"upstream":"a"`) || rec.Header().Get("X-Upstream") != "a" {
		t.Fatalf("healthy: %d %s", rec.Code, rec.Body)
	}
	s.Set(Settings{ErrorRate: 1})
	if rec := get(t, h, "/x"); rec.Code != 503 {
		t.Fatalf("error rate 1: %d", rec.Code)
	}
	if rec := get(t, h, "/healthz"); rec.Code != 200 {
		t.Fatalf("health check must ignore error rate: %d", rec.Code)
	}
	s.Set(Settings{Latency: 30 * time.Millisecond})
	start := time.Now()
	get(t, h, "/x")
	if time.Since(start) < 30*time.Millisecond {
		t.Fatal("latency not applied")
	}
	// Latency gives up when the client does.
	ctx, cancel := context.WithCancel(context.Background())
	s.Set(Settings{Latency: time.Minute})
	go func() { time.Sleep(10 * time.Millisecond); cancel() }()
	start = time.Now()
	h.ServeHTTP(httptest.NewRecorder(), httptest.NewRequest(http.MethodGet, "/x", nil).WithContext(ctx))
	if time.Since(start) > 5*time.Second {
		t.Fatal("latency ignored cancellation")
	}
	served, failed, _ := s.Counts()
	if served != 2 || failed != 1 {
		t.Fatalf("counts served %d failed %d", served, failed)
	}
}

func TestBlackholeHoldsUntilClientGivesUp(t *testing.T) {
	s := New("b")
	srv := httptest.NewServer(s.Handler())
	defer srv.Close()
	defer s.Close()
	s.Set(Settings{Blackhole: true})
	client := &http.Client{Timeout: 100 * time.Millisecond}
	start := time.Now()
	if _, err := client.Get(srv.URL + "/healthz"); err == nil {
		t.Fatal("blackhole answered")
	}
	if time.Since(start) < 100*time.Millisecond {
		t.Fatal("returned before the client timeout")
	}
	if _, _, held := s.Counts(); held != 1 {
		t.Fatalf("held %d", held)
	}
	// Close releases held requests.
	done := make(chan struct{})
	go func() {
		s.Handler().ServeHTTP(httptest.NewRecorder(), httptest.NewRequest(http.MethodGet, "/", nil))
		close(done)
	}()
	time.Sleep(10 * time.Millisecond)
	s.Close()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Close did not release the blackhole")
	}
}

func TestControl(t *testing.T) {
	s := New("c")
	c := s.Control()
	do := func(method, body string) (int, map[string]any) {
		rec := httptest.NewRecorder()
		c.ServeHTTP(rec, httptest.NewRequest(method, "/state", strings.NewReader(body)))
		var out map[string]any
		_ = json.Unmarshal(rec.Body.Bytes(), &out)
		return rec.Code, out
	}
	code, out := do(http.MethodPut, `{"error_rate":0.5,"latency":"5ms"}`)
	if code != 200 || out["settings"].(map[string]any)["error_rate"] != 0.5 || out["settings"].(map[string]any)["latency"] != "5ms" {
		t.Fatalf("%d %v", code, out)
	}
	if code, out = do(http.MethodGet, ""); code != 200 || out["name"] != "c" {
		t.Fatalf("%d %v", code, out)
	}
	if code, out = do(http.MethodPut, `{"error_rate":2}`); code != 400 || out["error"] == nil {
		t.Fatalf("%d %v", code, out)
	}
	if s.Settings().ErrorRate != 0.5 {
		t.Fatal("a rejected patch changed the settings")
	}
	if code, _ = do(http.MethodDelete, ""); code != 405 {
		t.Fatalf("DELETE: %d", code)
	}
	rec := httptest.NewRecorder()
	c.ServeHTTP(rec, httptest.NewRequest(http.MethodPut, "/state", io.NopCloser(strings.NewReader(strings.Repeat("x", MaxControlBody+10)))))
	if rec.Code != 400 {
		t.Fatalf("oversized: %d", rec.Code)
	}
}
