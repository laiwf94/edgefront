// Package proxy is steer's data plane: it routes each request to a cluster,
// enforces the cluster's concurrency limit, and forwards the request through
// net/http/httputil.ReverseProxy with a RoundTripper that picks endpoints,
// retries and feeds health signals back into outlier detection.
//
// Configuration is held in an immutable runtime swapped atomically on reload.
// Requests already in flight finish on the runtime they started with.
package proxy

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httputil"
	"net/netip"
	"sync"
	"sync/atomic"
	"time"

	"github.com/laiwf94/steer/internal/balancer"
	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
	"github.com/laiwf94/steer/internal/health"
	"github.com/laiwf94/steer/internal/metrics"
	"github.com/laiwf94/steer/internal/outlier"
	"github.com/laiwf94/steer/internal/reqid"
	"github.com/laiwf94/steer/internal/retry"
	"github.com/laiwf94/steer/internal/route"
)

const (
	requestIDHeader = reqid.Header
	// OverflowHeader is set on 503 responses refused by a cluster's
	// concurrency limit, naming the limit that was hit.
	OverflowHeader = "X-Steer-Overflow"
)

// Options configure a Server.
type Options struct {
	Logger  *slog.Logger
	Metrics *metrics.Registry
	// Clock drives health checking, outlier detection, retry budgets and
	// latency decay. Defaults to the wall clock.
	Clock clock.Clock
}

// Server is the proxy. Create it with New; it is an http.Handler.
type Server struct {
	log *slog.Logger
	m   *metrics.Registry
	clk clock.Clock
	rp  *httputil.ReverseProxy

	rt       atomic.Pointer[runtime]
	reloadMu sync.Mutex
	closed   bool

	reloadOK, reloadFailed *metrics.Counter
	noRoute                *metrics.Counter
}

// runtime is one loaded configuration. It is never modified after build.
type runtime struct {
	version  uint64
	loadedAt time.Time
	cfg      *config.Config
	router   *route.Router
	clusters map[string]*cluster
	order    []string
	trusted  []netip.Prefix

	stop context.CancelFunc
	wg   sync.WaitGroup
}

// Attempt results, used as metric labels.
const (
	resultSuccess = iota
	resultHTTP5xx
	resultConnectError
	resultTimeout
	resultReset
	numResults
)

var resultNames = [numResults]string{"success", "http_5xx", "connect_error", "timeout", "reset"}

type cluster struct {
	cfg       config.Cluster
	eps       []*endpoint.Endpoint
	byAddr    map[string]*endpoint.Endpoint
	lb        *balancer.LB
	detector  *outlier.Detector // nil when disabled
	checker   *health.Checker   // nil when disabled
	budget    *retry.Budget     // nil without retries
	breaker   *breaker
	transport *http.Transport
	retryOn   [100]bool // 500..599

	requests        [6]*metrics.Counter // by status class; 0 is unknown
	latency         *metrics.Histogram
	attempts        [][numResults]*metrics.Counter // by endpoint index
	retries         *metrics.Counter
	budgetExhausted *metrics.Counter
	overflow        *metrics.Counter
}

// breaker is the per-cluster concurrency limit. It survives reloads so the
// count of requests already in flight stays right.
type breaker struct {
	inflight atomic.Int64
	limit    atomic.Int64
}

func (b *breaker) acquire() bool {
	if b.inflight.Add(1) > b.limit.Load() {
		b.inflight.Add(-1)
		return false
	}
	return true
}

func (b *breaker) release() { b.inflight.Add(-1) }

// New builds a proxy for cfg and starts its health checkers and outlier
// detectors.
func New(cfg *config.Config, opts Options) (*Server, error) {
	s := &Server{log: opts.Logger, m: opts.Metrics, clk: opts.Clock}
	if s.log == nil {
		s.log = slog.New(slog.NewJSONHandler(io.Discard, nil))
	}
	if s.m == nil {
		s.m = metrics.New()
	}
	if s.clk == nil {
		s.clk = clock.Real{}
	}
	s.reloadOK = s.m.Counter("steer_config_reloads_total", `result="success"`, "Configuration reloads.")
	s.reloadFailed = s.m.Counter("steer_config_reloads_total", `result="failure"`, "Configuration reloads.")
	s.noRoute = s.m.Counter("steer_no_route_total", "", "Requests that matched no route.")
	s.m.Gauge("steer_config_version", "Version of the configuration being served.", func() float64 {
		return float64(s.rt.Load().version)
	})
	s.rp = &httputil.ReverseProxy{
		Rewrite:        s.rewrite,
		Transport:      roundTripper{s},
		ModifyResponse: s.modifyResponse,
		ErrorHandler:   s.errorHandler,
		ErrorLog:       slog.NewLogLogger(s.log.Handler(), slog.LevelWarn),
		BufferPool:     newBufferPool(),
	}
	rt, err := s.build(cfg, nil, 1)
	if err != nil {
		return nil, err
	}
	s.rt.Store(rt)
	s.start(rt)
	return s, nil
}

// Reload builds a runtime for cfg (already validated by config.Parse),
// carrying over endpoint state, concurrency counters, retry budgets and
// connection pools by cluster name and address, then swaps it in. On error
// the current configuration keeps serving.
func (s *Server) Reload(cfg *config.Config) error {
	s.reloadMu.Lock()
	defer s.reloadMu.Unlock()
	if s.closed {
		return errors.New("proxy is shut down")
	}
	old := s.rt.Load()
	if cfg.Listen != old.cfg.Listen || cfg.AdminListen != old.cfg.AdminListen {
		s.reloadFailed.Inc()
		return errors.New("listen and admin_listen cannot change on reload; restart steer instead")
	}
	rt, err := s.build(cfg, old, old.version+1)
	if err != nil {
		s.reloadFailed.Inc()
		return err
	}
	s.rt.Store(rt)
	old.stop()
	for name, oc := range old.clusters {
		if nc, ok := rt.clusters[name]; !ok || nc.transport != oc.transport {
			oc.transport.CloseIdleConnections()
		}
	}
	s.start(rt)
	s.reloadOK.Inc()
	s.log.Info("configuration loaded", "version", rt.version, "clusters", len(rt.clusters), "routes", len(cfg.Routes))
	return nil
}

// RecordFailedReload counts a reload that failed before reaching Reload,
// such as an unreadable or invalid file.
func (s *Server) RecordFailedReload() { s.reloadFailed.Inc() }

// Close stops health checking and outlier detection and closes idle upstream
// connections. Call it after the HTTP server has shut down.
func (s *Server) Close() {
	s.reloadMu.Lock()
	defer s.reloadMu.Unlock()
	if s.closed {
		return
	}
	s.closed = true
	rt := s.rt.Load()
	rt.stop()
	rt.wg.Wait()
	for _, c := range rt.clusters {
		c.transport.CloseIdleConnections()
	}
}

func (s *Server) start(rt *runtime) {
	ctx, cancel := context.WithCancel(context.Background())
	rt.stop = cancel
	for _, name := range rt.order {
		c := rt.clusters[name]
		if c.checker != nil {
			rt.wg.Go(func() { c.checker.Run(ctx) })
		}
		if c.detector != nil {
			rt.wg.Go(func() { c.detector.Run(ctx) })
		}
	}
}

func (s *Server) build(cfg *config.Config, old *runtime, version uint64) (*runtime, error) {
	rt := &runtime{version: version, loadedAt: s.clk.Now(), cfg: cfg, router: route.New(cfg.Routes),
		clusters: map[string]*cluster{}, trusted: cfg.TrustedProxies}
	for _, cc := range cfg.Clusters {
		var prev *cluster
		if old != nil {
			prev = old.clusters[cc.Name]
		}
		c, err := s.buildCluster(cc, prev)
		if err != nil {
			return nil, fmt.Errorf("cluster %s: %w", cc.Name, err)
		}
		rt.clusters[cc.Name] = c
		rt.order = append(rt.order, cc.Name)
	}
	return rt, nil
}

func (s *Server) buildCluster(cc config.Cluster, prev *cluster) (*cluster, error) {
	c := &cluster{cfg: cc, byAddr: map[string]*endpoint.Endpoint{}}
	for _, ec := range cc.Endpoints {
		ep := endpoint.New(ec.Address, ec.Weight, ec.Priority)
		if prev != nil {
			if old, ok := prev.byAddr[ec.Address]; ok {
				ep.State = old.State
			}
		}
		if cc.HealthCheck == nil && !ep.Healthy() {
			ep.SetHealthy(true) // nothing will ever probe it back up
		}
		if cc.OutlierDetection == nil {
			ep.ClearEjection()
		}
		c.eps = append(c.eps, ep)
		c.byAddr[ec.Address] = ep
	}
	lb, err := balancer.New(c.eps, balancer.Options{Policy: cc.Policy, PanicThreshold: cc.PanicThresholdPercent, Clock: s.clk})
	if err != nil {
		return nil, err
	}
	c.lb = lb

	if prev != nil {
		c.breaker = prev.breaker
	} else {
		c.breaker = &breaker{}
	}
	c.breaker.limit.Store(int64(cc.MaxRequests))

	if prev != nil && prev.cfg.ConnectTimeout == cc.ConnectTimeout {
		c.transport = prev.transport
	} else {
		c.transport = newTransport(cc.ConnectTimeout)
	}

	if r := cc.Retry; r != nil {
		if prev != nil && prev.cfg.Retry != nil && prev.cfg.Retry.Budget == r.Budget {
			c.budget = prev.budget
		} else {
			c.budget = retry.NewBudget(r.Budget.Ratio, r.Budget.MinPerSecond, r.Budget.Window, s.clk)
		}
		for _, code := range r.RetryOn {
			c.retryOn[code-500] = true
		}
	}

	name := cc.Name
	if od := cc.OutlierDetection; od != nil {
		c.detector = outlier.New(*od, c.eps, s.clk, func(ep *endpoint.Endpoint, reason string, until time.Time) {
			s.m.Counter("steer_outlier_ejections_total", fmt.Sprintf(`cluster=%q,reason=%q`, name, reason), "Endpoints ejected by outlier detection.").Inc()
			s.log.Warn("endpoint ejected", "cluster", name, "endpoint", ep.Address, "reason", reason,
				"ejected_for_ms", until.Sub(s.clk.Now()).Milliseconds())
		})
	}
	if hc := cc.HealthCheck; hc != nil {
		c.checker = health.New(*hc, c.eps, s.clk, func(ep *endpoint.Endpoint, healthy bool, reason string) {
			to := "unhealthy"
			if healthy {
				to = "healthy"
			}
			s.m.Counter("steer_health_transitions_total", fmt.Sprintf(`cluster=%q,to=%q`, name, to), "Active health check verdict changes.").Inc()
			if healthy {
				s.log.Info("endpoint healthy", "cluster", name, "endpoint", ep.Address)
			} else {
				s.log.Warn("endpoint unhealthy", "cluster", name, "endpoint", ep.Address, "reason", reason)
			}
		})
	}

	const reqHelp = "Requests by cluster and response status class."
	c.requests[0] = s.m.Counter("steer_requests_total", fmt.Sprintf(`cluster=%q,code_class="unknown"`, name), reqHelp)
	for k := 1; k <= 5; k++ {
		c.requests[k] = s.m.Counter("steer_requests_total", fmt.Sprintf(`cluster=%q,code_class="%dxx"`, name, k), reqHelp)
	}
	c.latency = s.m.Histogram("steer_request_duration_seconds", fmt.Sprintf(`cluster=%q`, name), "Request latency as seen by the client, including retries.")
	c.retries = s.m.Counter("steer_retries_total", fmt.Sprintf(`cluster=%q`, name), "Retry attempts sent.")
	c.budgetExhausted = s.m.Counter("steer_retry_budget_exhausted_total", fmt.Sprintf(`cluster=%q`, name), "Retries skipped because the retry budget was spent.")
	c.overflow = s.m.Counter("steer_overflow_total", fmt.Sprintf(`cluster=%q`, name), "Requests refused by the cluster concurrency limit.")
	c.attempts = make([][numResults]*metrics.Counter, len(c.eps))
	for i, ep := range c.eps {
		for r := range numResults {
			c.attempts[i][r] = s.m.Counter("steer_upstream_attempts_total",
				fmt.Sprintf(`cluster=%q,endpoint=%q,result=%q`, name, ep.Address, resultNames[r]), "Upstream attempts by endpoint and result.")
		}
	}
	return c, nil
}

func newTransport(connectTimeout time.Duration) *http.Transport {
	return &http.Transport{
		Proxy:                  nil, // never route through an environment-configured proxy
		DialContext:            (&net.Dialer{Timeout: connectTimeout, KeepAlive: 30 * time.Second}).DialContext,
		MaxIdleConns:           4096,
		MaxIdleConnsPerHost:    256,
		IdleConnTimeout:        90 * time.Second,
		ExpectContinueTimeout:  time.Second,
		MaxResponseHeaderBytes: 64 << 10,
		DisableCompression:     true, // pass encodings through untouched
	}
}

type stateKey struct{}

// reqState travels with the request so the RoundTripper, error handler and
// access log share what happened. It is only touched by the handler's
// goroutine (ReverseProxy calls RoundTrip synchronously).
type reqState struct {
	id       string
	rt       *runtime
	cluster  *cluster
	path     string
	tried    []string
	attempts int
	status   int
	results  []string  // outcome of each attempt
	failure  string    // why the proxy answered instead of an upstream
	body     io.Closer // the response body handed to ReverseProxy
}

func stateFrom(ctx context.Context) *reqState {
	st, _ := ctx.Value(stateKey{}).(*reqState)
	return st
}

// statusWriter remembers the status and size for logs and metrics. Unwrap
// lets http.ResponseController reach Flush and Hijack underneath.
type statusWriter struct {
	http.ResponseWriter
	code  int
	bytes int64
}

func (w *statusWriter) WriteHeader(code int) {
	if w.code == 0 && code >= 200 {
		w.code = code
	}
	w.ResponseWriter.WriteHeader(code)
}

func (w *statusWriter) Write(b []byte) (int, error) {
	if w.code == 0 {
		w.code = http.StatusOK
	}
	n, err := w.ResponseWriter.Write(b)
	w.bytes += int64(n)
	return n, err
}

func (w *statusWriter) Unwrap() http.ResponseWriter { return w.ResponseWriter }

// ServeHTTP proxies one request.
func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	start := time.Now()
	rt := s.rt.Load()
	st := &reqState{id: reqid.FromRequest(r), rt: rt, path: route.CleanPath(r.URL.Path)}
	sw := &statusWriter{ResponseWriter: w}
	defer func() {
		p := recover()
		s.finish(r, sw, st, start, p)
		if p != nil {
			panic(p) // http.ErrAbortHandler and friends: let net/http abort the connection
		}
	}()

	if r.Method == http.MethodConnect {
		st.failure = "connect_method"
		sw.Header().Set("Allow", "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS")
		s.writeError(sw, st, http.StatusMethodNotAllowed, "method_not_allowed", "CONNECT is not supported")
		return
	}
	rte, ok := rt.router.Match(r.Host, st.path)
	if !ok {
		s.noRoute.Inc()
		st.failure = "no_route"
		s.writeError(sw, st, http.StatusNotFound, "no_route", "no route matches this host and path")
		return
	}
	c := rt.clusters[rte.Cluster]
	st.cluster = c
	if !c.breaker.acquire() {
		c.overflow.Inc()
		st.failure = "overflow"
		sw.Header().Set(OverflowHeader, "cluster_max_requests")
		s.writeError(sw, st, http.StatusServiceUnavailable, "overloaded", "too many concurrent requests to this service")
		return
	}
	defer c.breaker.release()

	ctx := context.WithValue(r.Context(), stateKey{}, st)
	if c.cfg.Timeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, c.cfg.Timeout)
		defer cancel()
	}
	s.rp.ServeHTTP(sw, r.WithContext(ctx))
	if st.body != nil {
		// ReverseProxy leaves the body open on some failed protocol
		// upgrades; closing it again is harmless and releases the
		// endpoint's in-flight slot and the upstream connection.
		st.body.Close()
	}
}

func (s *Server) finish(r *http.Request, sw *statusWriter, st *reqState, start time.Time, panicked any) {
	d := time.Since(start)
	status := sw.code
	if status == 0 {
		status = st.status // a protocol upgrade writes its 101 on the hijacked connection
	}
	aborted := panicked != nil
	clusterName := ""
	if c := st.cluster; c != nil {
		clusterName = c.cfg.Name
		class := 0
		if status >= 100 && status < 600 {
			class = status / 100
		}
		c.requests[class].Inc()
		c.latency.Observe(d)
	}
	lvl := slog.LevelInfo
	if status >= 500 || aborted {
		lvl = slog.LevelWarn
	}
	attrs := []slog.Attr{
		slog.String("request_id", st.id),
		slog.String("client_ip", ClientIP(r.RemoteAddr, r.Header.Values("X-Forwarded-For"), st.rt.trusted)),
		slog.String("method", r.Method),
		slog.String("host", r.Host),
		slog.String("path", st.path),
		slog.String("cluster", clusterName),
		slog.Any("endpoints", st.tried),
		slog.Int("attempts", st.attempts),
		slog.Int("status", status),
		slog.Int64("bytes", sw.bytes),
		slog.Float64("duration_ms", float64(d.Microseconds())/1000),
	}
	if st.failure != "" {
		attrs = append(attrs, slog.String("failure", st.failure))
	}
	if len(st.results) > 0 {
		attrs = append(attrs, slog.Any("attempt_results", st.results))
	}
	if aborted {
		attrs = append(attrs, slog.Bool("aborted", true))
	}
	s.log.LogAttrs(r.Context(), lvl, "request", attrs...)
}

func (s *Server) rewrite(pr *httputil.ProxyRequest) {
	st := stateFrom(pr.In.Context())
	pr.Out.URL.Scheme = "http"
	pr.Out.URL.Host = st.cluster.cfg.Name // replaced with an endpoint address per attempt
	if st.path != pr.In.URL.Path {
		pr.Out.URL.Path, pr.Out.URL.RawPath = st.path, ""
	}
	pr.Out.Host = pr.In.Host
	setForwarded(pr, st.rt.trusted)
	pr.Out.Header.Set(requestIDHeader, st.id)
}

func (s *Server) modifyResponse(resp *http.Response) error {
	st := stateFrom(resp.Request.Context())
	st.status = resp.StatusCode
	resp.Header.Set(requestIDHeader, st.id)
	return nil
}

func (s *Server) errorHandler(w http.ResponseWriter, r *http.Request, err error) {
	st := stateFrom(r.Context())
	var ue *upstreamError
	switch {
	case errors.Is(err, errNoHealthyUpstream):
		st.failure = "no_healthy_upstream"
		s.writeError(w, st, http.StatusServiceUnavailable, "no_healthy_upstream", "no healthy upstream is available")
	case errors.Is(err, context.DeadlineExceeded):
		st.failure = "request_timeout"
		s.writeError(w, st, http.StatusGatewayTimeout, "upstream_timeout", "the upstream did not answer in time")
	case errors.Is(err, context.Canceled):
		st.failure = "client_canceled"
		w.WriteHeader(499) // nginx's "client closed request"; nobody is listening
	case errors.As(err, &ue) && ue.result == resultTimeout:
		st.failure = "upstream_timeout"
		s.writeError(w, st, http.StatusGatewayTimeout, "upstream_timeout", "the upstream did not answer in time")
	case errors.As(err, &ue) && ue.result == -1:
		st.failure = "client_body"
		s.writeError(w, st, http.StatusBadRequest, "bad_request", "could not read the request body")
	case errors.As(err, &ue):
		st.failure = resultNames[ue.result]
		s.writeError(w, st, http.StatusBadGateway, "bad_gateway", "the upstream failed to answer")
	default:
		st.failure = "proxy_error"
		s.log.Warn("proxy error", "request_id", st.id, "error", err.Error())
		s.writeError(w, st, http.StatusBadGateway, "bad_gateway", "the upstream failed to answer")
	}
}

func (s *Server) writeError(w http.ResponseWriter, st *reqState, status int, code, msg string) {
	h := w.Header()
	h.Set(requestIDHeader, st.id)
	h.Set("Content-Type", "application/json")
	h.Set("Cache-Control", "no-store")
	h.Set("X-Content-Type-Options", "nosniff")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{"error": map[string]string{"code": code, "message": msg, "request_id": st.id}})
}

// bufferPool recycles ReverseProxy's 32 KiB copy buffers.
type bufferPool struct{ p sync.Pool }

func newBufferPool() *bufferPool {
	return &bufferPool{p: sync.Pool{New: func() any { b := make([]byte, 32<<10); return &b }}}
}

func (b *bufferPool) Get() []byte  { return *(b.p.Get().(*[]byte)) }
func (b *bufferPool) Put(x []byte) { b.p.Put(&x) }

// State is the proxy's current view, served by the admin API.
type State struct {
	ConfigVersion uint64         `json:"config_version"`
	LoadedAt      time.Time      `json:"loaded_at"`
	Clusters      []ClusterState `json:"clusters"`
}

// ClusterState describes one cluster.
type ClusterState struct {
	Name               string          `json:"name"`
	Policy             config.Policy   `json:"policy"`
	InFlight           int64           `json:"in_flight"`
	MaxRequests        int             `json:"max_requests"`
	RetryBudgetBalance *float64        `json:"retry_budget_balance,omitempty"`
	Balancer           balancer.State  `json:"balancer"`
	Endpoints          []EndpointState `json:"endpoints"`
}

// EndpointState describes one endpoint.
type EndpointState struct {
	Address  string `json:"address"`
	Weight   int    `json:"weight"`
	Priority int    `json:"priority"`
	endpoint.Snapshot
}

// State returns the current view.
func (s *Server) State() State {
	rt := s.rt.Load()
	now := s.clk.Now()
	out := State{ConfigVersion: rt.version, LoadedAt: rt.loadedAt}
	for _, name := range rt.order {
		c := rt.clusters[name]
		cs := ClusterState{Name: name, Policy: c.cfg.Policy, InFlight: c.breaker.inflight.Load(),
			MaxRequests: c.cfg.MaxRequests, Balancer: c.lb.State()}
		if c.budget != nil {
			b := c.budget.Balance()
			cs.RetryBudgetBalance = &b
		}
		for _, ep := range c.eps {
			cs.Endpoints = append(cs.Endpoints, EndpointState{Address: ep.Address, Weight: ep.Weight,
				Priority: ep.Priority, Snapshot: ep.Snapshot(now)})
		}
		out.Clusters = append(out.Clusters, cs)
	}
	return out
}
