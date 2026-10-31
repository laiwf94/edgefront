package proxy

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/metrics"
)

// syncBuffer is a goroutine-safe log sink.
type syncBuffer struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (s *syncBuffer) Write(p []byte) (int, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.b.Write(p)
}

func (s *syncBuffer) lines() []map[string]any {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []map[string]any
	for _, l := range strings.Split(strings.TrimSpace(s.b.String()), "\n") {
		var m map[string]any
		if json.Unmarshal([]byte(l), &m) == nil {
			out = append(out, m)
		}
	}
	return out
}

func (s *syncBuffer) requests() []map[string]any {
	var out []map[string]any
	for _, l := range s.lines() {
		if l["msg"] == "request" {
			out = append(out, l)
		}
	}
	return out
}

type harness struct {
	t     *testing.T
	p     *Server
	srv   *httptest.Server
	m     *metrics.Registry
	logs  *syncBuffer
	cfg   *config.Config
	proxy string
}

func parse(t *testing.T, js string) *config.Config {
	t.Helper()
	cfg, err := config.Parse([]byte(js))
	if err != nil {
		t.Fatalf("config: %v\n%s", err, js)
	}
	return cfg
}

func newHarness(t *testing.T, js string) *harness {
	t.Helper()
	h := &harness{t: t, m: metrics.New(), logs: &syncBuffer{}, cfg: parse(t, js)}
	p, err := New(h.cfg, Options{Logger: slog.New(slog.NewJSONHandler(h.logs, nil)), Metrics: h.m})
	if err != nil {
		t.Fatal(err)
	}
	h.p = p
	h.srv = httptest.NewServer(p)
	h.proxy = h.srv.URL
	t.Cleanup(func() {
		h.srv.Close()
		p.Close()
	})
	return h
}

// cluster renders a one-cluster config. extra is spliced into the cluster.
func oneCluster(extra string, addrs ...string) string {
	var eps []string
	for _, a := range addrs {
		eps = append(eps, fmt.Sprintf(`{"address":%q}`, a))
	}
	if extra != "" {
		extra = "," + extra
	}
	return fmt.Sprintf(`{"listen":"127.0.0.1:0","routes":[{"cluster":"svc"}],
		"clusters":[{"name":"svc","endpoints":[%s]%s}]}`, strings.Join(eps, ","), extra)
}

func addr(s *httptest.Server) string { return s.Listener.Addr().String() }

func closedAddr(t *testing.T) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	a := ln.Addr().String()
	ln.Close()
	return a
}

func (h *harness) do(req *http.Request) (*http.Response, string) {
	h.t.Helper()
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		h.t.Fatal(err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	return resp, string(b)
}

func (h *harness) get(path string) (*http.Response, string) {
	h.t.Helper()
	req, _ := http.NewRequest(http.MethodGet, h.proxy+path, nil)
	return h.do(req)
}

func (h *harness) metric(series string) string {
	var b strings.Builder
	h.m.Write(&b)
	for _, l := range strings.Split(b.String(), "\n") {
		if strings.HasPrefix(l, series+" ") {
			return strings.TrimPrefix(l, series+" ")
		}
	}
	return ""
}

func TestForwardsRequestAndHeaders(t *testing.T) {
	var got atomic.Pointer[http.Request]
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got.Store(r.Clone(context.Background()))
		w.Header().Set("X-Request-Id", "upstream-made-this-up")
		w.Header().Set("Connection", "X-Secret")
		w.Header().Set("X-Secret", "hop")
		io.WriteString(w, "hello")
	}))
	defer up.Close()
	h := newHarness(t, oneCluster("", addr(up)))

	req, _ := http.NewRequest(http.MethodGet, h.proxy+"/public/../admin/x?q=1", nil)
	req.Host = "shop.example.com"
	req.Header.Set("X-Forwarded-For", "6.6.6.6")
	req.Header.Set("X-Forwarded-Proto", "https")
	req.Header.Set("X-Forwarded-Host", "evil.example")
	req.Header.Set("X-Request-Id", "client-id-1")
	req.Header.Set("Connection", "X-Hop")
	req.Header.Set("X-Hop", "drop me")
	resp, body := h.do(req)
	if resp.StatusCode != 200 || body != "hello" {
		t.Fatalf("%d %q", resp.StatusCode, body)
	}
	if resp.Header.Get("X-Request-Id") != "client-id-1" || len(resp.Header.Values("X-Request-Id")) != 1 {
		t.Fatalf("response request id %v", resp.Header.Values("X-Request-Id"))
	}
	if resp.Header.Get("X-Secret") != "" {
		t.Fatal("hop-by-hop response header leaked")
	}
	r := got.Load()
	checks := map[string][2]string{
		"Host":              {r.Host, "shop.example.com"},
		"path":              {r.URL.Path, "/admin/x"},
		"query":             {r.URL.RawQuery, "q=1"},
		"X-Forwarded-For":   {r.Header.Get("X-Forwarded-For"), "127.0.0.1"},
		"X-Forwarded-Proto": {r.Header.Get("X-Forwarded-Proto"), "http"},
		"X-Forwarded-Host":  {r.Header.Get("X-Forwarded-Host"), "shop.example.com"},
		"X-Request-Id":      {r.Header.Get("X-Request-Id"), "client-id-1"},
		"X-Hop":             {r.Header.Get("X-Hop"), ""},
	}
	for name, c := range checks {
		if c[0] != c[1] {
			t.Errorf("%s = %q, want %q", name, c[0], c[1])
		}
	}

	// An unsafe request id is replaced, and the replacement is forwarded.
	req, _ = http.NewRequest(http.MethodGet, h.proxy+"/", nil)
	req.Header.Set("X-Request-Id", "bad id\twith tab")
	resp, _ = h.do(req)
	id := resp.Header.Get("X-Request-Id")
	if id == "" || strings.ContainsAny(id, " \t") || got.Load().Header.Get("X-Request-Id") != id {
		t.Fatalf("replacement id %q, upstream saw %q", id, got.Load().Header.Get("X-Request-Id"))
	}
}

func TestTrustedProxyChain(t *testing.T) {
	var xff, proto, host atomic.Value
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		xff.Store(r.Header.Get("X-Forwarded-For"))
		proto.Store(r.Header.Get("X-Forwarded-Proto"))
		host.Store(r.Header.Get("X-Forwarded-Host"))
	}))
	defer up.Close()
	js := strings.Replace(oneCluster("", addr(up)), `"routes"`, `"trusted_proxies":["127.0.0.0/8"],"routes"`, 1)
	h := newHarness(t, js)
	req, _ := http.NewRequest(http.MethodGet, h.proxy+"/", nil)
	req.Header.Add("X-Forwarded-For", "203.0.113.9, junk")
	req.Header.Add("X-Forwarded-For", "10.1.1.1")
	req.Header.Set("X-Forwarded-Proto", "HTTPS")
	req.Header.Set("X-Forwarded-Host", "www.example.com")
	h.do(req)
	if xff.Load() != "203.0.113.9, 10.1.1.1, 127.0.0.1" || proto.Load() != "https" || host.Load() != "www.example.com" {
		t.Fatalf("xff %v proto %v host %v", xff.Load(), proto.Load(), host.Load())
	}
	logs := h.logs.requests()
	if logs[len(logs)-1]["client_ip"] != "10.1.1.1" {
		t.Fatalf("client_ip %v", logs[len(logs)-1]["client_ip"])
	}
	// Malformed proto and host from a trusted proxy fall back to our own view.
	req, _ = http.NewRequest(http.MethodGet, h.proxy+"/", nil)
	req.Header.Set("X-Forwarded-Proto", "gopher")
	req.Header.Set("X-Forwarded-Host", "a b")
	h.do(req)
	if proto.Load() != "http" || host.Load() == "a b" {
		t.Fatalf("proto %v host %v", proto.Load(), host.Load())
	}
}

func TestNoRoute(t *testing.T) {
	up := httptest.NewServer(http.NotFoundHandler())
	defer up.Close()
	js := strings.Replace(oneCluster("", addr(up)), `"routes":[{"cluster":"svc"}]`, `"routes":[{"cluster":"svc","host":"a.example.com"}]`, 1)
	h := newHarness(t, js)
	resp, body := h.get("/")
	var e struct {
		Error struct {
			Code      string `json:"code"`
			Message   string `json:"message"`
			RequestID string `json:"request_id"`
		}
	}
	json.Unmarshal([]byte(body), &e)
	if resp.StatusCode != 404 || e.Error.Code != "no_route" || e.Error.RequestID != resp.Header.Get("X-Request-Id") || e.Error.RequestID == "" {
		t.Fatalf("%d %s", resp.StatusCode, body)
	}
	if h.metric("steer_no_route_total") != "1" {
		t.Fatal("no_route metric")
	}
}

const retryAll = `"retry":{"max_retries":2,"backoff_base":"1ms","backoff_max":"2ms"}`

func TestRetryOnConnectFailure(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		io.WriteString(w, "ok:"+string(b))
	}))
	defer up.Close()
	dead := closedAddr(t)
	h := newHarness(t, oneCluster(retryAll, dead, addr(up)))
	for i := 0; i < 20; i++ {
		// POST without an idempotency key: still retried after a connect
		// error, because the request never left.
		resp, body := h.do(mustReq(http.MethodPost, h.proxy+"/x", "payload"))
		if resp.StatusCode != 200 || body != "ok:payload" {
			t.Fatalf("request %d: %d %q", i, resp.StatusCode, body)
		}
	}
	if v := h.metric(fmt.Sprintf(`steer_upstream_attempts_total{cluster="svc",endpoint=%q,result="connect_error"}`, dead)); v == "0" || v == "" {
		t.Fatalf("connect errors not counted: %q", v)
	}
	if h.metric(`steer_requests_total{cluster="svc",code_class="2xx"}`) != "20" {
		t.Fatal("2xx count")
	}
}

func mustReq(method, url, body string) *http.Request {
	req, _ := http.NewRequest(method, url, strings.NewReader(body))
	return req
}

// countingUpstream answers with status after recording the body it saw.
type countingUpstream struct {
	srv    *httptest.Server
	hits   atomic.Int32
	status atomic.Int32
	bodies chan string
}

func newCounting(status int) *countingUpstream {
	c := &countingUpstream{bodies: make(chan string, 100)}
	c.status.Store(int32(status))
	c.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c.hits.Add(1)
		b, _ := io.ReadAll(r.Body)
		select {
		case c.bodies <- string(b):
		default:
		}
		w.WriteHeader(int(c.status.Load()))
		fmt.Fprintf(w, "status %d", c.status.Load())
	}))
	return c
}

func TestRetryOnStatusRespectsIdempotency(t *testing.T) {
	bad := newCounting(503)
	defer bad.srv.Close()
	good := newCounting(200)
	defer good.srv.Close()
	// Round robin over [bad, good]: the first request goes to bad first.
	h := newHarness(t, oneCluster(retryAll, addr(bad.srv), addr(good.srv)))

	resp, _ := h.get("/")
	if resp.StatusCode != 200 || bad.hits.Load() != 1 || good.hits.Load() != 1 {
		t.Fatalf("GET: %d, bad %d good %d", resp.StatusCode, bad.hits.Load(), good.hits.Load())
	}
	// POST without a key: the 503 is returned as is.
	bad.hits.Store(0)
	good.hits.Store(0)
	statuses := map[int]int{}
	for range 2 {
		resp, _ = h.do(mustReq(http.MethodPost, h.proxy+"/", "x"))
		statuses[resp.StatusCode]++
	}
	if statuses[503] != 1 || statuses[200] != 1 || bad.hits.Load() != 1 || good.hits.Load() != 1 {
		t.Fatalf("POST: %v bad %d good %d", statuses, bad.hits.Load(), good.hits.Load())
	}
	// POST with Idempotency-Key: retried, with the same body.
	for len(good.bodies) > 0 {
		<-good.bodies
	}
	for range 2 {
		req := mustReq(http.MethodPost, h.proxy+"/", "same-body")
		req.Header.Set("Idempotency-Key", "k-1")
		if resp, _ = h.do(req); resp.StatusCode != 200 {
			t.Fatalf("POST with key: %d", resp.StatusCode)
		}
	}
	for range 2 {
		if b := <-good.bodies; b != "same-body" {
			t.Fatalf("replayed body %q", b)
		}
	}
	if h.metric(`steer_retries_total{cluster="svc"}`) != "2" {
		t.Fatalf("retries %s", h.metric(`steer_retries_total{cluster="svc"}`))
	}
}

func TestNeverTheSameEndpointTwice(t *testing.T) {
	a, b := newCounting(503), newCounting(502)
	defer a.srv.Close()
	defer b.srv.Close()
	h := newHarness(t, oneCluster(`"retry":{"max_retries":5,"backoff_base":"1ms","backoff_max":"1ms"}`, addr(a.srv), addr(b.srv)))
	resp, body := h.get("/")
	if a.hits.Load() != 1 || b.hits.Load() != 1 {
		t.Fatalf("hits a %d b %d", a.hits.Load(), b.hits.Load())
	}
	if resp.StatusCode != 502 || body != "status 502" {
		t.Fatalf("last upstream answer should pass through: %d %q", resp.StatusCode, body)
	}
	l := h.logs.requests()[0]
	if l["attempts"].(float64) != 2 || len(l["endpoints"].([]any)) != 2 {
		t.Fatalf("log %v", l)
	}
}

func TestRetryOnlyListedStatuses(t *testing.T) {
	a, b := newCounting(500), newCounting(200)
	defer a.srv.Close()
	defer b.srv.Close()
	h := newHarness(t, oneCluster(retryAll, addr(a.srv), addr(b.srv)))
	if resp, _ := h.get("/"); resp.StatusCode != 500 || b.hits.Load() != 0 {
		t.Fatalf("500 is not in retry_on: %d, b hits %d", resp.StatusCode, b.hits.Load())
	}
}

func TestLargeBodyIsNotRetried(t *testing.T) {
	a, b := newCounting(503), newCounting(200)
	defer a.srv.Close()
	defer b.srv.Close()
	h := newHarness(t, oneCluster(`"retry":{"max_body_bytes":8,"backoff_base":"1ms"}`, addr(a.srv), addr(b.srv)))
	req := mustReq(http.MethodPut, h.proxy+"/", "0123456789abcdef")
	req.ContentLength = -1 // chunked: the size is only discovered while buffering
	resp, _ := h.do(req)
	if resp.StatusCode != 503 || b.hits.Load() != 0 {
		t.Fatalf("large body retried: %d, b hits %d", resp.StatusCode, b.hits.Load())
	}
	if got := <-a.bodies; got != "0123456789abcdef" {
		t.Fatalf("upstream got %q", got)
	}
	// Exactly at the limit is buffered and retried.
	resp, _ = h.do(mustReq(http.MethodPut, h.proxy+"/", "01234567"))
	if resp.StatusCode != 200 {
		t.Fatalf("small body: %d", resp.StatusCode)
	}
}

func TestRetryBudgetStopsAmplification(t *testing.T) {
	a, b := newCounting(503), newCounting(503)
	defer a.srv.Close()
	defer b.srv.Close()
	h := newHarness(t, oneCluster(`"retry":{"backoff_base":"1ms","budget":{"ratio":0.1,"min_per_second":0,"window":"10s"}}`,
		addr(a.srv), addr(b.srv)))
	for range 100 {
		h.get("/")
	}
	// 100 requests buy at most 10 retries.
	total := a.hits.Load() + b.hits.Load()
	if total < 100 || total > 110 {
		t.Fatalf("upstream saw %d attempts for 100 requests", total)
	}
	if h.metric(`steer_retry_budget_exhausted_total{cluster="svc"}`) == "0" {
		t.Fatal("budget exhaustion not counted")
	}
}

func TestPerTryTimeoutRetriesSlowHeaders(t *testing.T) {
	release := make(chan struct{})
	slow := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		select {
		case <-release:
		case <-r.Context().Done():
		}
	}))
	defer slow.Close()
	defer close(release)
	fast := newCounting(200)
	defer fast.srv.Close()
	h := newHarness(t, oneCluster(`"retry":{"per_try_timeout":"50ms","backoff_base":"1ms"}`, addr(slow), addr(fast.srv)))
	start := time.Now()
	resp, _ := h.get("/")
	if resp.StatusCode != 200 || time.Since(start) > 2*time.Second {
		t.Fatalf("%d after %v", resp.StatusCode, time.Since(start))
	}
	if h.metric(fmt.Sprintf(`steer_upstream_attempts_total{cluster="svc",endpoint=%q,result="timeout"}`, addr(slow))) != "1" {
		t.Fatal("timeout attempt not counted")
	}
}

func TestPerTryTimeoutDoesNotCutStreams(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
		for i := 0; i < 5; i++ {
			fmt.Fprintf(w, "chunk%d;", i)
			w.(http.Flusher).Flush()
			time.Sleep(30 * time.Millisecond)
		}
	}))
	defer up.Close()
	h := newHarness(t, oneCluster(`"retry":{"per_try_timeout":"50ms"}`, addr(up)))
	resp, body := h.get("/")
	if resp.StatusCode != 200 || body != "chunk0;chunk1;chunk2;chunk3;chunk4;" {
		t.Fatalf("%d %q", resp.StatusCode, body)
	}
}

func TestOverallTimeout(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { <-r.Context().Done() }))
	defer up.Close()
	h := newHarness(t, oneCluster(`"timeout":"100ms",`+retryAll, addr(up)))
	resp, body := h.get("/")
	if resp.StatusCode != 504 || !strings.Contains(body, "upstream_timeout") {
		t.Fatalf("%d %s", resp.StatusCode, body)
	}
	// The endpoint was not blamed: no outlier input, result recorded as canceled.
	if l := h.logs.requests()[0]; l["failure"] != "request_timeout" {
		t.Fatalf("log %v", l)
	}
}

func TestAllAttemptsFailWithoutResponse(t *testing.T) {
	h := newHarness(t, oneCluster(retryAll, closedAddr(t), closedAddr(t)))
	resp, body := h.get("/")
	if resp.StatusCode != 502 || !strings.Contains(body, "bad_gateway") {
		t.Fatalf("%d %s", resp.StatusCode, body)
	}
	if l := h.logs.requests()[0]; l["failure"] != "connect_error" || l["attempts"].(float64) != 2 {
		t.Fatalf("log %v", l)
	}
}

func TestUpstreamResetIsRetriedForGet(t *testing.T) {
	reset := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, _, err := http.NewResponseController(w).Hijack()
		if err == nil {
			conn.Close()
		}
	}))
	defer reset.Close()
	good := newCounting(200)
	defer good.srv.Close()
	h := newHarness(t, oneCluster(retryAll, addr(reset), addr(good.srv)))
	if resp, _ := h.get("/"); resp.StatusCode != 200 {
		t.Fatalf("GET after reset: %d", resp.StatusCode)
	}
	if v := h.metric(fmt.Sprintf(`steer_upstream_attempts_total{cluster="svc",endpoint=%q,result="reset"}`, addr(reset))); v != "1" {
		t.Fatalf("reset count %q", v)
	}
	// A POST is not retried after a reset: the upstream may have acted on
	// it. Round robin sends one of two POSTs to the resetting endpoint.
	statuses := map[int]int{}
	for range 2 {
		resp, _ := h.do(mustReq(http.MethodPost, h.proxy+"/", "x"))
		statuses[resp.StatusCode]++
	}
	if statuses[502] != 1 || statuses[200] != 1 {
		t.Fatalf("POST statuses %v", statuses)
	}
}

func TestConcurrencyLimit(t *testing.T) {
	release := make(chan struct{})
	entered := make(chan struct{}, 1)
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		entered <- struct{}{}
		<-release
	}))
	defer up.Close()
	h := newHarness(t, oneCluster(`"max_requests":1`, addr(up)))
	done := make(chan int)
	go func() {
		resp, _ := h.get("/")
		done <- resp.StatusCode
	}()
	<-entered
	resp, body := h.get("/")
	if resp.StatusCode != 503 || resp.Header.Get(OverflowHeader) != "cluster_max_requests" || !strings.Contains(body, "overloaded") {
		t.Fatalf("%d %v %s", resp.StatusCode, resp.Header, body)
	}
	if st := h.p.State(); st.Clusters[0].InFlight != 1 {
		t.Fatalf("in flight %d", st.Clusters[0].InFlight)
	}
	close(release)
	if code := <-done; code != 200 {
		t.Fatalf("first request %d", code)
	}
	if h.metric(`steer_overflow_total{cluster="svc"}`) != "1" {
		t.Fatal("overflow metric")
	}
}

func TestNoHealthyUpstream(t *testing.T) {
	up := newCounting(503) // health check fails too
	defer up.srv.Close()
	h := newHarness(t, oneCluster(`"panic_threshold_percent":0,
		"health_check":{"path":"/healthz","interval":"20ms","timeout":"10ms","unhealthy_threshold":1}`, addr(up.srv)))
	deadline := time.Now().Add(5 * time.Second)
	for {
		resp, body := h.get("/")
		if resp.StatusCode == 503 && strings.Contains(body, "no_healthy_upstream") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("still routing to an unhealthy endpoint: %d %s", resp.StatusCode, body)
		}
		time.Sleep(10 * time.Millisecond)
	}
	if st := h.p.State(); st.Clusters[0].Endpoints[0].Healthy || st.Clusters[0].Endpoints[0].LastProbeError == "" {
		t.Fatalf("state %+v", st.Clusters[0].Endpoints[0])
	}
	if h.metric(`steer_health_transitions_total{cluster="svc",to="unhealthy"}`) != "1" {
		t.Fatal("transition metric")
	}
}

func TestOutlierEjectionThroughTheProxy(t *testing.T) {
	bad, good := newCounting(503), newCounting(200)
	defer bad.srv.Close()
	defer good.srv.Close()
	h := newHarness(t, oneCluster(`"outlier_detection":{"consecutive_gateway_failures":3,"max_ejection_percent":50},`+retryAll,
		addr(bad.srv), addr(good.srv)))
	for range 20 {
		if resp, _ := h.get("/"); resp.StatusCode != 200 {
			t.Fatalf("status %d", resp.StatusCode)
		}
	}
	if bad.hits.Load() != 3 {
		t.Fatalf("bad endpoint saw %d requests; ejection after 3 expected", bad.hits.Load())
	}
	ep := h.p.State().Clusters[0].Endpoints[0]
	if !ep.Ejected || ep.TimesEjected != 1 || ep.LastEjectReason != "consecutive_gateway_failure" {
		t.Fatalf("state %+v", ep)
	}
	if h.metric(`steer_outlier_ejections_total{cluster="svc",reason="consecutive_gateway_failure"}`) != "1" {
		t.Fatal("ejection metric")
	}
	logged := false
	for _, l := range h.logs.lines() {
		logged = logged || l["msg"] == "endpoint ejected"
	}
	if !logged {
		t.Fatal("ejection not logged")
	}
}

func TestMaglevKeepsUsersOnOneEndpoint(t *testing.T) {
	var ups []*countingUpstream
	var addrs []string
	for range 4 {
		u := newCounting(200)
		defer u.srv.Close()
		ups = append(ups, u)
		addrs = append(addrs, addr(u.srv))
	}
	h := newHarness(t, oneCluster(`"policy":"maglev","hash_header":"X-User"`, addrs...))
	for user := 0; user < 20; user++ {
		before := make([]int32, 4)
		for i, u := range ups {
			before[i] = u.hits.Load()
		}
		for range 5 {
			req, _ := http.NewRequest(http.MethodGet, h.proxy+"/", nil)
			req.Header.Set("X-User", fmt.Sprint("user-", user))
			h.do(req)
		}
		moved := 0
		for i, u := range ups {
			if d := u.hits.Load() - before[i]; d != 0 {
				moved++
				if d != 5 {
					t.Fatalf("user %d split across endpoints", user)
				}
			}
		}
		if moved != 1 {
			t.Fatalf("user %d touched %d endpoints", user, moved)
		}
	}
}

func TestReloadKeepsEndpointState(t *testing.T) {
	a, b, c := newCounting(200), newCounting(200), newCounting(200)
	defer a.srv.Close()
	defer b.srv.Close()
	defer c.srv.Close()
	od := `"outlier_detection":{"max_ejection_percent":100}`
	h := newHarness(t, oneCluster(od, addr(a.srv), addr(b.srv)))
	st := h.p.State()
	if st.ConfigVersion != 1 {
		t.Fatalf("version %d", st.ConfigVersion)
	}
	// Eject a by hand through its state, then reload with a kept and c added.
	h.p.rt.Load().clusters["svc"].eps[0].Eject(time.Now(), time.Hour, time.Hour, "test")
	if err := h.p.Reload(parse(t, oneCluster(od+`,"max_requests":7`, addr(a.srv), addr(c.srv)))); err != nil {
		t.Fatal(err)
	}
	st = h.p.State()
	cl := st.Clusters[0]
	if st.ConfigVersion != 2 || cl.MaxRequests != 7 || len(cl.Endpoints) != 2 {
		t.Fatalf("after reload %+v", st)
	}
	if !cl.Endpoints[0].Ejected || cl.Endpoints[0].TimesEjected != 1 {
		t.Fatalf("kept endpoint lost its state: %+v", cl.Endpoints[0])
	}
	if cl.Endpoints[1].Ejected || cl.Endpoints[1].Address != addr(c.srv) {
		t.Fatalf("new endpoint %+v", cl.Endpoints[1])
	}
	for range 10 {
		h.get("/")
	}
	if a.hits.Load() != 0 || c.hits.Load() != 10 || b.hits.Load() != 0 {
		t.Fatalf("hits a %d b %d c %d", a.hits.Load(), b.hits.Load(), c.hits.Load())
	}
	// Turning outlier detection off clears ejections.
	if err := h.p.Reload(parse(t, oneCluster("", addr(a.srv), addr(c.srv)))); err != nil {
		t.Fatal(err)
	}
	if h.p.State().Clusters[0].Endpoints[0].Ejected {
		t.Fatal("ejection survived turning outlier detection off")
	}
	if h.metric(`steer_config_reloads_total{result="success"}`) != "2" || h.metric("steer_config_version") != "3" {
		t.Fatal("reload metrics")
	}
}

func TestReloadRejectsListenerChanges(t *testing.T) {
	up := newCounting(200)
	defer up.srv.Close()
	h := newHarness(t, oneCluster("", addr(up.srv)))
	changed := strings.Replace(oneCluster("", addr(up.srv)), "127.0.0.1:0", "127.0.0.1:1", 1)
	if err := h.p.Reload(parse(t, changed)); err == nil || !strings.Contains(err.Error(), "cannot change") {
		t.Fatalf("err %v", err)
	}
	if h.p.State().ConfigVersion != 1 || h.metric(`steer_config_reloads_total{result="failure"}`) != "1" {
		t.Fatal("failed reload changed state")
	}
	h.p.Close()
	h.p.Close() // idempotent
	if err := h.p.Reload(parse(t, oneCluster("", addr(up.srv)))); err == nil {
		t.Fatal("reload after close accepted")
	}
}

func TestReloadRestoresHealthWhenChecksRemoved(t *testing.T) {
	up := newCounting(503)
	defer up.srv.Close()
	hc := `"panic_threshold_percent":0,"health_check":{"path":"/","interval":"20ms","timeout":"10ms","unhealthy_threshold":1}`
	h := newHarness(t, oneCluster(hc, addr(up.srv)))
	deadline := time.Now().Add(5 * time.Second)
	for h.p.State().Clusters[0].Endpoints[0].Healthy {
		if time.Now().After(deadline) {
			t.Fatal("never marked unhealthy")
		}
		time.Sleep(5 * time.Millisecond)
	}
	if err := h.p.Reload(parse(t, oneCluster(`"panic_threshold_percent":0`, addr(up.srv)))); err != nil {
		t.Fatal(err)
	}
	if !h.p.State().Clusters[0].Endpoints[0].Healthy {
		t.Fatal("endpoint stuck unhealthy with no health checker to revive it")
	}
}

func TestWebSocketStyleUpgrade(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Upgrade") != "echo" {
			http.Error(w, "want upgrade", 400)
			return
		}
		conn, brw, err := http.NewResponseController(w).Hijack()
		if err != nil {
			return
		}
		defer conn.Close()
		brw.WriteString("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n")
		brw.Flush()
		line, _ := brw.ReadString('\n')
		brw.WriteString("echo:" + line)
		brw.Flush()
	}))
	defer up.Close()
	h := newHarness(t, oneCluster(retryAll, addr(up)))
	conn, err := net.Dial("tcp", h.srv.Listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	fmt.Fprintf(conn, "GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n")
	br := bufio.NewReader(conn)
	resp, err := http.ReadResponse(br, nil)
	if err != nil || resp.StatusCode != 101 {
		t.Fatalf("%v %v", resp, err)
	}
	fmt.Fprintf(conn, "ping\n")
	conn.SetReadDeadline(time.Now().Add(5 * time.Second))
	line, err := br.ReadString('\n')
	if err != nil || line != "echo:ping\n" {
		t.Fatalf("%q %v", line, err)
	}
	conn.Close()
	deadline := time.Now().Add(5 * time.Second)
	for {
		logs := h.logs.requests()
		if len(logs) == 1 {
			if logs[0]["status"].(float64) != 101 {
				t.Fatalf("log %v", logs[0])
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("upgrade never logged")
		}
		time.Sleep(5 * time.Millisecond)
	}
	if ep := h.p.State().Clusters[0].Endpoints[0]; ep.InFlight != 0 {
		t.Fatalf("in-flight leaked: %d", ep.InFlight)
	}
}

func TestServerSentEventsFlushImmediately(t *testing.T) {
	release := make(chan struct{})
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, "data: first\n\n")
		w.(http.Flusher).Flush()
		<-release
		io.WriteString(w, "data: second\n\n")
	}))
	defer up.Close()
	defer close(release)
	h := newHarness(t, oneCluster("", addr(up)))
	resp, err := http.Get(h.proxy + "/events")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	br := bufio.NewReader(resp.Body)
	got := make(chan string, 1)
	go func() {
		line, _ := br.ReadString('\n')
		got <- line
	}()
	select {
	case line := <-got:
		if line != "data: first\n" {
			t.Fatalf("%q", line)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("first event was buffered")
	}
}

func TestClientCancel(t *testing.T) {
	entered := make(chan struct{})
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		close(entered)
		<-r.Context().Done()
	}))
	defer up.Close()
	h := newHarness(t, oneCluster(retryAll, addr(up)))
	ctx, cancel := context.WithCancel(context.Background())
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, h.proxy+"/", nil)
	go func() { <-entered; cancel() }()
	if _, err := http.DefaultClient.Do(req); err == nil {
		t.Fatal("expected an error")
	}
	deadline := time.Now().Add(5 * time.Second)
	for len(h.logs.requests()) == 0 {
		if time.Now().After(deadline) {
			t.Fatal("no access log")
		}
		time.Sleep(5 * time.Millisecond)
	}
	if l := h.logs.requests()[0]; l["status"].(float64) != 499 || l["failure"] != "client_canceled" {
		t.Fatalf("log %v", l)
	}
	if v := h.metric(fmt.Sprintf(`steer_upstream_attempts_total{cluster="svc",endpoint=%q,result="timeout"}`, addr(up))); v != "0" {
		t.Fatalf("client cancel blamed on the endpoint: %s", v)
	}
}

func TestClientBodyReadError(t *testing.T) {
	up := newCounting(200)
	defer up.srv.Close()
	h := newHarness(t, oneCluster(retryAll, addr(up.srv)))
	conn, err := net.Dial("tcp", h.srv.Listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	// Promise 100 bytes, send 3 with a malformed chunk: the body read fails.
	fmt.Fprintf(conn, "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nabc\r\n")
	resp, err := http.ReadResponse(bufio.NewReader(conn), nil)
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != 400 || up.hits.Load() != 0 {
		t.Fatalf("status %d, upstream hits %d", resp.StatusCode, up.hits.Load())
	}
}

func TestAccessLogAndState(t *testing.T) {
	up := newCounting(200)
	defer up.srv.Close()
	h := newHarness(t, oneCluster(retryAll, closedAddr(t), addr(up.srv)))
	req, _ := http.NewRequest(http.MethodGet, h.proxy+"/a/b", nil)
	req.Header.Set("X-Request-Id", "trace-77")
	h.do(req)
	l := h.logs.requests()[0]
	for k, want := range map[string]any{"request_id": "trace-77", "method": "GET", "path": "/a/b", "cluster": "svc",
		"status": float64(200), "attempts": float64(2), "client_ip": "127.0.0.1"} {
		if l[k] != want {
			t.Errorf("%s = %v, want %v", k, l[k], want)
		}
	}
	if r := l["attempt_results"].([]any); len(r) != 2 || r[0] != "connect_error" || r[1] != "success" {
		t.Fatalf("attempt_results %v", r)
	}
	if _, ok := l["duration_ms"].(float64); !ok {
		t.Fatal("duration_ms")
	}
	st := h.p.State()
	if c := st.Clusters[0]; c.Policy != "round_robin" || c.RetryBudgetBalance == nil || len(c.Balancer.PriorityLoad) != 1 {
		t.Fatalf("state %+v", c)
	}
	if b, err := json.Marshal(st); err != nil || !strings.Contains(string(b), `"ewma_latency_ms"`) {
		t.Fatalf("%s %v", b, err)
	}
}

func TestBufferPool(t *testing.T) {
	p := newBufferPool()
	b := p.Get()
	if len(b) != 32<<10 {
		t.Fatalf("len %d", len(b))
	}
	p.Put(b)
}

func TestClassify(t *testing.T) {
	ctx := context.Background()
	if classify(&http.Response{StatusCode: 404}, nil, ctx) != resultSuccess || classify(&http.Response{StatusCode: 500}, nil, ctx) != resultHTTP5xx {
		t.Fatal("status classes")
	}
	if classify(nil, &net.OpError{Op: "dial", Err: fmt.Errorf("refused")}, ctx) != resultConnectError {
		t.Fatal("dial")
	}
	if classify(nil, timeoutErr{}, ctx) != resultTimeout {
		t.Fatal("net timeout")
	}
	if classify(nil, io.ErrUnexpectedEOF, ctx) != resultReset {
		t.Fatal("reset")
	}
	if !sleep(ctx, 0) {
		t.Fatal("zero sleep")
	}
	done, cancel := context.WithCancel(ctx)
	cancel()
	if sleep(done, time.Hour) || sleep(done, 0) {
		t.Fatal("sleep ignored cancellation")
	}
}

type timeoutErr struct{}

func (timeoutErr) Error() string   { return "i/o timeout" }
func (timeoutErr) Timeout() bool   { return true }
func (timeoutErr) Temporary() bool { return true }

// Regression: a host that fails fast must not look like the fastest host to
// the least-loaded policy.
func TestLeastLoadedAvoidsFastFailures(t *testing.T) {
	bad := newCounting(503)
	defer bad.srv.Close()
	good := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { time.Sleep(2 * time.Millisecond) }))
	defer good.Close()
	h := newHarness(t, oneCluster(`"policy":"least_loaded"`, addr(bad.srv), addr(good)))
	for range 100 {
		h.get("/")
	}
	if n := bad.hits.Load(); n > 3 {
		t.Fatalf("fast-failing host got %d of 100 requests", n)
	}
}

func TestConnectIsRejected(t *testing.T) {
	up := newCounting(200)
	defer up.srv.Close()
	h := newHarness(t, oneCluster("", addr(up.srv)))
	conn, err := net.Dial("tcp", h.srv.Listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	fmt.Fprintf(conn, "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
	resp, err := http.ReadResponse(bufio.NewReader(conn), nil)
	if err != nil || resp.StatusCode != 405 || up.hits.Load() != 0 {
		t.Fatalf("%v %v hits %d", resp, err, up.hits.Load())
	}
}

// Regression: when the request deadline passes during backoff, the client
// gets a clean 504 rather than a response whose body can no longer be read.
func TestDeadlineDuringBackoff(t *testing.T) {
	a, b := newCounting(503), newCounting(503)
	defer a.srv.Close()
	defer b.srv.Close()
	h := newHarness(t, oneCluster(`"timeout":"50ms","retry":{"max_retries":1,"backoff_base":"1s","backoff_max":"1s"}`, addr(a.srv), addr(b.srv)))
	timeouts := 0
	for range 5 {
		resp, body := h.get("/")
		switch {
		case resp.StatusCode == 504 && strings.Contains(body, "upstream_timeout"):
			timeouts++
		case resp.StatusCode == 503 && body == "status 503":
			// the jittered backoff happened to be shorter than the deadline
		default:
			t.Fatalf("%d %q", resp.StatusCode, body)
		}
	}
	if timeouts == 0 {
		t.Fatal("deadline never hit during backoff")
	}
}

func TestReloadStopsOldHealthChecker(t *testing.T) {
	var probes atomic.Int32
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/healthz" {
			probes.Add(1)
		}
	}))
	defer up.Close()
	h := newHarness(t, oneCluster(`"health_check":{"path":"/healthz","interval":"10ms","timeout":"5ms"}`, addr(up)))
	deadline := time.Now().Add(5 * time.Second)
	for probes.Load() < 3 {
		if time.Now().After(deadline) {
			t.Fatal("no probes")
		}
		time.Sleep(5 * time.Millisecond)
	}
	if err := h.p.Reload(parse(t, oneCluster("", addr(up)))); err != nil {
		t.Fatal(err)
	}
	time.Sleep(30 * time.Millisecond) // a probe already in flight may land
	before := probes.Load()
	time.Sleep(100 * time.Millisecond)
	if after := probes.Load(); after != before {
		t.Fatalf("old health checker still probing: %d -> %d", before, after)
	}
}

// Regression: an upstream that answers an upgrade with the wrong protocol
// must not leak the endpoint's in-flight slot or its connection.
func TestFailedUpgradeReleasesEndpoint(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, brw, err := http.NewResponseController(w).Hijack()
		if err != nil {
			return
		}
		defer conn.Close()
		brw.WriteString("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: other\r\n\r\n")
		brw.Flush()
		io.Copy(io.Discard, brw) // hold the connection until the proxy closes it
	}))
	defer up.Close()
	h := newHarness(t, oneCluster("", addr(up)))
	req, _ := http.NewRequest(http.MethodGet, h.proxy+"/ws", nil)
	req.Header.Set("Connection", "Upgrade")
	req.Header.Set("Upgrade", "echo")
	resp, _ := h.do(req)
	if resp.StatusCode != 502 {
		t.Fatalf("status %d", resp.StatusCode)
	}
	if ep := h.p.State().Clusters[0].Endpoints[0]; ep.InFlight != 0 {
		t.Fatalf("in-flight leaked: %d", ep.InFlight)
	}
}

// Regression: a status above 599 must not be treated as a 5xx (it used to
// index past the retry_on table).
func TestStatusAbove599(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, brw, err := http.NewResponseController(w).Hijack()
		if err != nil {
			return
		}
		defer conn.Close()
		brw.WriteString("HTTP/1.1 642 Odd\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
		brw.Flush()
	}))
	defer up.Close()
	h := newHarness(t, oneCluster(retryAll, addr(up)))
	resp, body := h.get("/")
	if resp.StatusCode != 642 || body != "ok" {
		t.Fatalf("%d %q", resp.StatusCode, body)
	}
}
