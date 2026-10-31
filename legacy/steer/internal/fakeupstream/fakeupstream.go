// Package fakeupstream is an HTTP upstream whose failures can be dialled in
// at run time: added latency, a fraction of 503 responses, and a blackhole
// mode that accepts requests and never answers. cmd/upstream serves it for
// demos; the proxy's tests use it for failure injection.
//
// /healthz answers 200 subject to latency and blackhole but not to the error
// rate, like a real service whose health endpoint stays green while a
// dependency fails half its requests. That is the case only passive outlier
// detection can catch.
package fakeupstream

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math/rand/v2"
	"net/http"
	"sync"
	"sync/atomic"
	"time"
)

// Limits on control requests and settings.
const (
	MaxControlBody = 4 << 10
	MaxLatency     = time.Minute
	maxRequestBody = 10 << 20
)

// Settings are the injected faults.
type Settings struct {
	Latency   time.Duration
	ErrorRate float64 // fraction of requests answered with 503, 0 to 1
	Blackhole bool
}

type wireSettings struct {
	Latency   string   `json:"latency"`
	ErrorRate *float64 `json:"error_rate"`
	Blackhole *bool    `json:"blackhole"`
}

// MarshalJSON renders latency as a duration string.
func (s Settings) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]any{"latency": s.Latency.String(), "error_rate": s.ErrorRate, "blackhole": s.Blackhole})
}

// ParseSettings applies a JSON patch such as {"error_rate":0.5} on top of
// cur. Unknown fields, trailing data and out-of-range values are rejected.
func ParseSettings(cur Settings, data []byte) (Settings, error) {
	if len(data) > MaxControlBody {
		return cur, fmt.Errorf("body is larger than %d bytes", MaxControlBody)
	}
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	var w wireSettings
	if err := dec.Decode(&w); err != nil {
		return cur, fmt.Errorf("invalid JSON: %v", err)
	}
	if dec.More() {
		return cur, errors.New("body must be a single JSON object")
	}
	next := cur
	if w.Latency != "" {
		d, err := time.ParseDuration(w.Latency)
		if err != nil || d < 0 || d > MaxLatency {
			return cur, fmt.Errorf("latency must be a duration between 0s and %s", MaxLatency)
		}
		next.Latency = d
	}
	if w.ErrorRate != nil {
		if !(*w.ErrorRate >= 0 && *w.ErrorRate <= 1) {
			return cur, errors.New("error_rate must be between 0 and 1")
		}
		next.ErrorRate = *w.ErrorRate
	}
	if w.Blackhole != nil {
		next.Blackhole = *w.Blackhole
	}
	return next, nil
}

// Server is one fake upstream.
type Server struct {
	name     string
	mu       sync.Mutex
	settings Settings
	closed   chan struct{}
	once     sync.Once

	served, failed, held atomic.Int64
}

// New returns a healthy upstream that identifies itself as name.
func New(name string) *Server { return &Server{name: name, closed: make(chan struct{})} }

// Settings returns the current faults.
func (s *Server) Settings() Settings {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.settings
}

// Set replaces the faults.
func (s *Server) Set(v Settings) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.settings = v
}

// Close releases requests held by blackhole mode.
func (s *Server) Close() { s.once.Do(func() { close(s.closed) }) }

// Counts returns requests answered, requests failed on purpose, and
// requests held by the blackhole.
func (s *Server) Counts() (served, failed, held int64) {
	return s.served.Load(), s.failed.Load(), s.held.Load()
}

// Handler serves traffic.
func (s *Server) Handler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.Copy(io.Discard, io.LimitReader(r.Body, maxRequestBody))
		cfg := s.Settings()
		if cfg.Blackhole {
			s.held.Add(1)
			select { // never answer; wait for the client to give up
			case <-r.Context().Done():
			case <-s.closed:
			}
			return
		}
		if cfg.Latency > 0 {
			t := time.NewTimer(cfg.Latency)
			select {
			case <-t.C:
			case <-r.Context().Done():
				t.Stop()
				return
			}
		}
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("X-Upstream", s.name)
		if r.URL.Path == "/healthz" {
			_, _ = io.WriteString(w, `{"status":"ok"}`+"\n")
			return
		}
		if cfg.ErrorRate > 0 && rand.Float64() < cfg.ErrorRate {
			s.failed.Add(1)
			w.WriteHeader(http.StatusServiceUnavailable)
			_ = json.NewEncoder(w).Encode(map[string]string{"error": "injected failure", "upstream": s.name})
			return
		}
		s.served.Add(1)
		_ = json.NewEncoder(w).Encode(map[string]string{"upstream": s.name, "method": r.Method, "path": r.URL.Path})
	})
}

// Control serves GET /state and PUT /state with a JSON patch of Settings.
func (s *Server) Control() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/state", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch r.Method {
		case http.MethodGet:
		case http.MethodPut:
			data, err := io.ReadAll(io.LimitReader(r.Body, MaxControlBody+1))
			if err == nil {
				var next Settings
				if next, err = ParseSettings(s.Settings(), data); err == nil {
					s.Set(next)
				}
			}
			if err != nil {
				w.WriteHeader(http.StatusBadRequest)
				_ = json.NewEncoder(w).Encode(map[string]any{"error": map[string]string{"code": "invalid_argument", "message": err.Error()}})
				return
			}
		default:
			w.Header().Set("Allow", "GET, PUT")
			w.WriteHeader(http.StatusMethodNotAllowed)
			_ = json.NewEncoder(w).Encode(map[string]any{"error": map[string]string{"code": "method_not_allowed", "message": "use GET or PUT"}})
			return
		}
		served, failed, held := s.Counts()
		_ = json.NewEncoder(w).Encode(map[string]any{"name": s.name, "settings": s.Settings(),
			"served": served, "failed": failed, "held": held})
	})
	return mux
}
