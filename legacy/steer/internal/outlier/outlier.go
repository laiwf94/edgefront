// Package outlier ejects misbehaving endpoints based on the responses they
// actually return, following Envoy's outlier detection.
//
// Two detectors run per cluster:
//
//   - Consecutive gateway failures: an endpoint that returns N connect
//     errors, resets, timeouts or 502/503/504 in a row is ejected at once.
//   - Success rate: every interval, among endpoints that served at least the
//     minimum request volume, any whose success rate is below
//     mean - k x stdev is ejected. It needs a minimum number of such endpoints
//     so that the statistics mean something.
//
// An ejection lasts base x (times ejected), capped at the maximum; the
// multiplier decays by one for each base period the endpoint then stays
// healthy. No more than max_ejection_percent of the cluster is ejected at
// once, but at least one endpoint may always be ejected when the percent is
// above zero.
package outlier

import (
	"cmp"
	"context"
	"math"
	"slices"
	"sync"
	"time"

	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
)

// Ejection reasons, used in logs and metrics.
const (
	ReasonConsecutiveGatewayFailure = "consecutive_gateway_failure"
	ReasonSuccessRate               = "success_rate"
)

// EjectFunc is told about every ejection.
type EjectFunc func(ep *endpoint.Endpoint, reason string, until time.Time)

// Detector runs outlier detection for one cluster. It is safe for concurrent
// use.
type Detector struct {
	cfg     config.OutlierDetection
	eps     []*endpoint.Endpoint
	clk     clock.Clock
	onEject EjectFunc
	mu      sync.Mutex // serialises ejection decisions so the cap holds
}

// New returns a detector over eps. onEject may be nil.
func New(cfg config.OutlierDetection, eps []*endpoint.Endpoint, clk clock.Clock, onEject EjectFunc) *Detector {
	if clk == nil {
		clk = clock.Real{}
	}
	if onEject == nil {
		onEject = func(*endpoint.Endpoint, string, time.Time) {}
	}
	return &Detector{cfg: cfg, eps: eps, clk: clk, onEject: onEject}
}

// Report records the outcome of one attempt against ep.
func (d *Detector) Report(ep *endpoint.Endpoint, gatewayFailure, failure bool) {
	run := ep.RecordResult(gatewayFailure, failure)
	if d.cfg.ConsecutiveGatewayFailures == 0 || run < d.cfg.ConsecutiveGatewayFailures {
		return
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	now := d.clk.Now()
	if !ep.Ejected(now) && d.canEject(now) {
		d.eject(ep, now, ReasonConsecutiveGatewayFailure)
	}
}

// canEject applies max_ejection_percent. Callers hold d.mu.
func (d *Detector) canEject(now time.Time) bool {
	limit := len(d.eps) * d.cfg.MaxEjectionPercent / 100
	if d.cfg.MaxEjectionPercent > 0 && limit < 1 {
		limit = 1
	}
	ejected := 0
	for _, ep := range d.eps {
		if ep.Ejected(now) {
			ejected++
		}
	}
	return ejected < limit
}

func (d *Detector) eject(ep *endpoint.Endpoint, now time.Time, reason string) {
	until := ep.Eject(now, d.cfg.BaseEjectionTime, d.cfg.MaxEjectionTime, reason)
	d.onEject(ep, reason, until)
}

// Tick runs the per-interval work: decay ejection multipliers, then evaluate
// success rates over the interval that just ended and reset the counters.
func (d *Detector) Tick() {
	d.mu.Lock()
	defer d.mu.Unlock()
	now := d.clk.Now()
	type sample struct {
		ep   *endpoint.Endpoint
		rate float64
	}
	var samples []sample
	for _, ep := range d.eps {
		ep.Decay(now, d.cfg.BaseEjectionTime)
		ok, failed := ep.TakeInterval()
		if d.cfg.SuccessRateMinimumHosts == 0 || ep.Ejected(now) {
			continue
		}
		if total := ok + failed; total >= uint64(d.cfg.SuccessRateRequestVolume) {
			samples = append(samples, sample{ep, float64(ok) / float64(total)})
		}
	}
	if d.cfg.SuccessRateMinimumHosts == 0 || len(samples) < d.cfg.SuccessRateMinimumHosts {
		return
	}
	var sum float64
	for _, s := range samples {
		sum += s.rate
	}
	mean := sum / float64(len(samples))
	var sq float64
	for _, s := range samples {
		sq += (s.rate - mean) * (s.rate - mean)
	}
	stdev := math.Sqrt(sq / float64(len(samples)))
	if stdev < 1e-9 {
		return // everyone is equally good (or bad): nothing is an outlier
	}
	threshold := mean - d.cfg.SuccessRateStdevFactor*stdev
	// Worst first, so the ejection cap removes the clearest outliers.
	slices.SortStableFunc(samples, func(a, b sample) int { return cmp.Compare(a.rate, b.rate) })
	for _, s := range samples {
		if s.rate >= threshold {
			break
		}
		if !d.canEject(now) {
			return
		}
		d.eject(s.ep, now, ReasonSuccessRate)
	}
}

// Run calls Tick every interval until ctx ends.
func (d *Detector) Run(ctx context.Context) {
	for {
		select {
		case <-ctx.Done():
			return
		case <-d.clk.After(d.cfg.Interval):
			d.Tick()
		}
	}
}
