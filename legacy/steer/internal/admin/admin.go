// Package admin serves steer's operator API: health, readiness, metrics,
// live cluster state and configuration reload.
//
// /healthz and /readyz are always open so load balancers can probe them.
// Everything else requires "Authorization: Bearer <token>" when a token is
// configured; the caller (cmd/steer) refuses to bind a non-loopback admin
// address without one.
package admin

import (
	"crypto/sha256"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"strings"
	"time"

	"github.com/laiwf94/steer/internal/metrics"
	"github.com/laiwf94/steer/internal/reqid"
)

// MinTokenLength is the shortest accepted admin token.
const MinTokenLength = 16

// Config configures the admin handler.
type Config struct {
	Token   string // empty: no authentication (loopback only)
	Logger  *slog.Logger
	Metrics *metrics.Registry
	State   func() any
	// Reload re-reads the configuration file and applies it. It returns the
	// new configuration version.
	Reload func() (uint64, error)
	// Ready reports false while the process is draining.
	Ready func() bool
}

// Handler is the admin API.
type Handler struct {
	cfg   Config
	token [sha256.Size]byte
	auth  bool
	mux   *http.ServeMux
}

// New returns the admin handler.
func New(cfg Config) (*Handler, error) {
	if cfg.State == nil || cfg.Reload == nil || cfg.Ready == nil || cfg.Metrics == nil {
		return nil, errors.New("admin: State, Reload, Ready and Metrics are required")
	}
	if cfg.Logger == nil {
		cfg.Logger = slog.New(slog.NewJSONHandler(io.Discard, nil))
	}
	h := &Handler{cfg: cfg, mux: http.NewServeMux()}
	if cfg.Token != "" {
		if len(cfg.Token) < MinTokenLength {
			return nil, fmt.Errorf("admin: token must be at least %d characters", MinTokenLength)
		}
		h.token, h.auth = sha256.Sum256([]byte(cfg.Token)), true
	}
	h.handle("/healthz", http.MethodGet, false, h.healthz)
	h.handle("/readyz", http.MethodGet, false, h.readyz)
	h.handle("/metrics", http.MethodGet, true, h.metrics)
	h.handle("/clusters", http.MethodGet, true, h.clusters)
	h.handle("/reload", http.MethodPost, true, h.reload)
	h.mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		writeError(w, http.StatusNotFound, "not_found", "no such endpoint")
	})
	return h, nil
}

// ServeHTTP implements http.Handler.
func (h *Handler) ServeHTTP(w http.ResponseWriter, r *http.Request) { h.mux.ServeHTTP(w, r) }

func (h *Handler) handle(path, method string, auth bool, fn http.HandlerFunc) {
	h.mux.HandleFunc(path, func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		id := reqid.FromRequest(r)
		sw := &statusWriter{ResponseWriter: w, id: id}
		defer func() {
			h.cfg.Logger.Info("admin request", "request_id", id, "method", r.Method, "path", path, "status", sw.code,
				"remote", r.RemoteAddr, "duration_ms", float64(time.Since(start).Microseconds())/1000)
		}()
		sw.Header().Set(reqid.Header, id)
		sw.Header().Set("Cache-Control", "no-store")
		sw.Header().Set("X-Content-Type-Options", "nosniff")
		if auth && h.auth && !h.authorized(r) {
			sw.Header().Set("WWW-Authenticate", `Bearer realm="steer-admin"`)
			writeError(sw, http.StatusUnauthorized, "unauthenticated", "missing or invalid admin token")
			return
		}
		if r.Method != method {
			sw.Header().Set("Allow", method)
			writeError(sw, http.StatusMethodNotAllowed, "method_not_allowed", "use "+method)
			return
		}
		fn(sw, r)
	})
}

// authorized compares SHA-256 digests in constant time, so neither the
// token's length nor a matching prefix shows in the timing.
func (h *Handler) authorized(r *http.Request) bool {
	tok, ok := strings.CutPrefix(r.Header.Get("Authorization"), "Bearer ")
	if !ok || tok == "" {
		return false
	}
	sum := sha256.Sum256([]byte(tok))
	return subtle.ConstantTimeCompare(sum[:], h.token[:]) == 1
}

func (h *Handler) healthz(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
}

func (h *Handler) readyz(w http.ResponseWriter, r *http.Request) {
	if !h.cfg.Ready() {
		writeJSON(w, http.StatusServiceUnavailable, map[string]string{"status": "draining"})
		return
	}
	writeJSON(w, http.StatusOK, map[string]string{"status": "ready"})
}

func (h *Handler) metrics(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain; version=0.0.4")
	if err := h.cfg.Metrics.Write(w); err != nil {
		h.cfg.Logger.Warn("writing metrics", "error", err.Error())
	}
}

func (h *Handler) clusters(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, h.cfg.State())
}

func (h *Handler) reload(w http.ResponseWriter, r *http.Request) {
	version, err := h.cfg.Reload()
	if err != nil {
		h.cfg.Logger.Warn("reload rejected", "error", err.Error())
		writeError(w, http.StatusBadRequest, "invalid_config", err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"status": "reloaded", "config_version": version})
}

type statusWriter struct {
	http.ResponseWriter
	code int
	id   string
}

func (w *statusWriter) WriteHeader(code int) {
	if w.code == 0 {
		w.code = code
	}
	w.ResponseWriter.WriteHeader(code)
}

func (w *statusWriter) Write(b []byte) (int, error) {
	if w.code == 0 {
		w.code = http.StatusOK
	}
	return w.ResponseWriter.Write(b)
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	enc := json.NewEncoder(w)
	enc.SetIndent("", "  ")
	_ = enc.Encode(v)
}

func writeError(w http.ResponseWriter, status int, code, msg string) {
	id := ""
	if sw, ok := w.(*statusWriter); ok {
		id = sw.id
	} else {
		id = reqid.New()
		w.Header().Set(reqid.Header, id)
	}
	writeJSON(w, status, map[string]any{"error": map[string]string{"code": code, "message": msg, "request_id": id}})
}
