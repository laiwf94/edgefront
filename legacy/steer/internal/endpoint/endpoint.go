// Package endpoint holds the live state of one upstream address: in-flight
// requests, latency, the active health verdict and passive outlier counters.
//
// State is kept apart from configuration so that a reload which keeps an
// address keeps everything steer has learned about it.
package endpoint

import (
	"math"
	"sync"
	"sync/atomic"
	"time"
)

// Endpoint is one configured upstream address.
type Endpoint struct {
	Address  string
	Weight   int
	Priority int
	*State
}

// New returns an endpoint with fresh state.
func New(address string, weight, priority int) *Endpoint {
	return &Endpoint{Address: address, Weight: weight, Priority: priority, State: NewState()}
}

// LatencyDecay is the time constant of the peak-EWMA latency estimate.
const LatencyDecay = 10 * time.Second

// State is the mutable, reload-surviving part of an endpoint. Fields read on
// every request are atomics; the rest sit behind mu.
type State struct {
	inflight     atomic.Int64
	healthy      atomic.Bool   // active health check verdict
	ejectedUntil atomic.Int64  // unix nanoseconds; outlier ejection ends then
	ewmaBits     atomic.Uint64 // seconds, as of ewmaAt
	ewmaAt       atomic.Int64  // unix nanoseconds

	mu sync.Mutex
	// Active health checking.
	hcSuccesses, hcFailures int
	lastProbeError          string
	// Passive outlier detection.
	consecutiveGW         int
	successes, failures   uint64 // since the last interval
	timesEjected          int
	lastEjectReason       string
	decayFrom             time.Time
	hasLatencyObservation bool
}

// NewState returns state for an endpoint that is assumed healthy until a
// health check says otherwise.
func NewState() *State {
	s := &State{}
	s.healthy.Store(true)
	return s
}

// Acquire records the start of a request to the endpoint.
func (s *State) Acquire() { s.inflight.Add(1) }

// Release records the end of a request started with Acquire.
func (s *State) Release() { s.inflight.Add(-1) }

// InFlight returns the number of requests outstanding.
func (s *State) InFlight() int64 { return s.inflight.Load() }

// ObserveLatency folds one response time into the peak-EWMA estimate used by
// the least-loaded policy (Finagle and Linkerd's "peak EWMA"): a slower sample
// replaces the estimate immediately, a faster one pulls it down with time
// constant LatencyDecay.
func (s *State) ObserveLatency(d time.Duration, now time.Time) {
	sample := d.Seconds()
	s.mu.Lock()
	defer s.mu.Unlock()
	cur := math.Float64frombits(s.ewmaBits.Load())
	at := time.Unix(0, s.ewmaAt.Load())
	if !s.hasLatencyObservation || sample > cur {
		cur = sample
	} else {
		w := decayWeight(now.Sub(at))
		cur = cur*w + sample*(1-w)
	}
	s.hasLatencyObservation = true
	s.ewmaAt.Store(now.UnixNano())
	s.ewmaBits.Store(math.Float64bits(cur))
}

func decayWeight(elapsed time.Duration) float64 {
	return math.Exp(-float64(max(elapsed, 0)) / float64(LatencyDecay))
}

// Latency returns the latency estimate in seconds as of now; zero before any
// sample. Between samples the estimate decays towards zero, as in Linkerd's
// implementation, so an endpoint that was penalised and then left alone is
// eventually tried, and measured, again.
func (s *State) Latency(now time.Time) float64 {
	cur := math.Float64frombits(s.ewmaBits.Load())
	if cur == 0 {
		return 0
	}
	return cur * decayWeight(now.Sub(time.Unix(0, s.ewmaAt.Load())))
}

// Cost is the least-loaded score: latency estimate times (in-flight + 1). An
// endpoint with no samples yet scores as if it took one microsecond, so new
// endpoints receive traffic and get measured.
func (s *State) Cost(now time.Time) float64 {
	return max(s.Latency(now), 1e-6) * float64(s.inflight.Load()+1)
}

// Healthy reports the active health check verdict.
func (s *State) Healthy() bool { return s.healthy.Load() }

// SetHealthy overrides the verdict, used when health checking is turned off.
func (s *State) SetHealthy(v bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.healthy.Store(v)
	s.hcSuccesses, s.hcFailures = 0, 0
	s.lastProbeError = ""
}

// Ejected reports whether outlier detection has ejected the endpoint at now.
func (s *State) Ejected(now time.Time) bool { return now.UnixNano() < s.ejectedUntil.Load() }

// EjectedUntil returns when the current or last ejection ends.
func (s *State) EjectedUntil() time.Time {
	n := s.ejectedUntil.Load()
	if n == 0 {
		return time.Time{}
	}
	return time.Unix(0, n)
}

// Available reports whether the load balancer may send requests here.
func (s *State) Available(now time.Time) bool { return s.Healthy() && !s.Ejected(now) }

// RecordProbe applies one health check result. The verdict flips to healthy
// after healthyAfter consecutive passes and to unhealthy after unhealthyAfter
// consecutive failures. It returns the verdict and whether it changed.
func (s *State) RecordProbe(ok bool, errText string, healthyAfter, unhealthyAfter int) (healthy, changed bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	was := s.healthy.Load()
	if ok {
		s.hcFailures = 0
		s.hcSuccesses++
		s.lastProbeError = ""
		if !was && s.hcSuccesses >= healthyAfter {
			s.healthy.Store(true)
		}
	} else {
		s.hcSuccesses = 0
		s.hcFailures++
		s.lastProbeError = errText
		if was && s.hcFailures >= unhealthyAfter {
			s.healthy.Store(false)
		}
	}
	now := s.healthy.Load()
	return now, now != was
}

// RecordResult counts one upstream attempt. gatewayFailure is a connect
// error, reset, timeout or 502/503/504; failure is any of those or any other
// 5xx. It returns the current run of consecutive gateway failures.
func (s *State) RecordResult(gatewayFailure, failure bool) int {
	s.mu.Lock()
	defer s.mu.Unlock()
	if gatewayFailure {
		s.consecutiveGW++
	} else {
		s.consecutiveGW = 0
	}
	if failure || gatewayFailure {
		s.failures++
	} else {
		s.successes++
	}
	return s.consecutiveGW
}

// TakeInterval returns and resets the attempt counts of the current interval.
func (s *State) TakeInterval() (successes, failures uint64) {
	s.mu.Lock()
	defer s.mu.Unlock()
	successes, failures = s.successes, s.failures
	s.successes, s.failures = 0, 0
	return successes, failures
}

// Eject removes the endpoint from load balancing for base x (times ejected),
// capped at maxTime, and returns when the ejection ends.
func (s *State) Eject(now time.Time, base, maxTime time.Duration, reason string) time.Time {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.timesEjected++
	d := time.Duration(min(int64(s.timesEjected), int64(maxTime/base)+1)) * base
	d = min(d, maxTime)
	until := now.Add(d)
	s.ejectedUntil.Store(until.UnixNano())
	s.consecutiveGW = 0
	s.lastEjectReason = reason
	s.decayFrom = until
	return until
}

// ClearEjection ends any ejection and forgets the multiplier, used when
// outlier detection is turned off for the endpoint's cluster.
func (s *State) ClearEjection() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.ejectedUntil.Store(0)
	s.timesEjected = 0
	s.consecutiveGW = 0
}

// Decay lowers the ejection multiplier by one for each full period the
// endpoint has spent un-ejected since its last ejection ended, so a host that
// has recovered starts again from the base ejection time.
func (s *State) Decay(now time.Time, period time.Duration) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.timesEjected == 0 || now.UnixNano() < s.ejectedUntil.Load() {
		return
	}
	for s.timesEjected > 0 && now.Sub(s.decayFrom) >= period {
		s.timesEjected--
		s.decayFrom = s.decayFrom.Add(period)
	}
}

// TimesEjected returns the current ejection multiplier.
func (s *State) TimesEjected() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.timesEjected
}

// Snapshot is a point-in-time copy of the state for the admin API.
type Snapshot struct {
	Healthy             bool      `json:"healthy"`
	LastProbeError      string    `json:"last_probe_error,omitempty"`
	Ejected             bool      `json:"ejected"`
	EjectedUntil        time.Time `json:"ejected_until,omitzero"`
	TimesEjected        int       `json:"times_ejected"`
	LastEjectReason     string    `json:"last_eject_reason,omitempty"`
	ConsecutiveFailures int       `json:"consecutive_gateway_failures"`
	InFlight            int64     `json:"in_flight"`
	LatencyMs           float64   `json:"ewma_latency_ms"`
	Available           bool      `json:"available"`
}

// Snapshot copies the state as of now.
func (s *State) Snapshot(now time.Time) Snapshot {
	s.mu.Lock()
	defer s.mu.Unlock()
	return Snapshot{
		Healthy:             s.healthy.Load(),
		LastProbeError:      s.lastProbeError,
		Ejected:             s.Ejected(now),
		EjectedUntil:        s.EjectedUntil(),
		TimesEjected:        s.timesEjected,
		LastEjectReason:     s.lastEjectReason,
		ConsecutiveFailures: s.consecutiveGW,
		InFlight:            s.inflight.Load(),
		LatencyMs:           math.Round(s.Latency(now)*1e6) / 1e3,
		Available:           s.healthy.Load() && !s.Ejected(now),
	}
}
