// Package metrics exposes counters and latency histograms in the Prometheus
// text format, without third-party dependencies.
package metrics

import (
	"fmt"
	"io"
	"math"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// Registry holds every metric. The zero value is not usable; call New.
type Registry struct {
	mu         sync.Mutex
	counters   map[string]*Counter
	histograms map[string]*Histogram
	gauges     map[string]func() float64
	help       map[string]string
}

// New returns an empty registry.
func New() *Registry {
	return &Registry{counters: map[string]*Counter{}, histograms: map[string]*Histogram{},
		gauges: map[string]func() float64{}, help: map[string]string{}}
}

// Counter is a monotonically increasing count.
type Counter struct{ v atomic.Uint64 }

// Inc adds one.
func (c *Counter) Inc() { c.v.Add(1) }

// Add adds n.
func (c *Counter) Add(n uint64) { c.v.Add(n) }

// Value returns the current count.
func (c *Counter) Value() uint64 { return c.v.Load() }

// Histogram counts observations into fixed buckets (seconds).
type Histogram struct {
	bounds []float64
	counts []atomic.Uint64 // len(bounds)+1, last is +Inf
	sumNs  atomic.Uint64
	total  atomic.Uint64
}

// DefaultLatencyBuckets covers 50µs to 2.5s.
var DefaultLatencyBuckets = []float64{0.00005, 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1, 2.5}

// Observe records one duration.
func (h *Histogram) Observe(d time.Duration) {
	s := d.Seconds()
	i := sort.SearchFloat64s(h.bounds, s)
	h.counts[i].Add(1)
	h.sumNs.Add(uint64(d.Nanoseconds()))
	h.total.Add(1)
}

// Count returns the number of observations.
func (h *Histogram) Count() uint64 { return h.total.Load() }

// Counter returns the counter with this name and labels, creating it.
// name must be a valid Prometheus metric name; labels are rendered as given,
// e.g. `cluster="web",code_class="2xx"`.
func (r *Registry) Counter(name, labels, help string) *Counter {
	r.mu.Lock()
	defer r.mu.Unlock()
	k := series(name, labels)
	c, ok := r.counters[k]
	if !ok {
		c = &Counter{}
		r.counters[k] = c
		r.help[name] = help
	}
	return c
}

// Histogram returns the histogram with this name and labels, creating it.
func (r *Registry) Histogram(name, labels, help string) *Histogram {
	r.mu.Lock()
	defer r.mu.Unlock()
	k := series(name, labels)
	h, ok := r.histograms[k]
	if !ok {
		h = &Histogram{bounds: DefaultLatencyBuckets, counts: make([]atomic.Uint64, len(DefaultLatencyBuckets)+1)}
		r.histograms[k] = h
		r.help[name] = help
	}
	return h
}

// Gauge registers a function read at scrape time.
func (r *Registry) Gauge(name, help string, fn func() float64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.gauges[name] = fn
	r.help[name] = help
}

func series(name, labels string) string {
	if labels == "" {
		return name
	}
	return name + "{" + labels + "}"
}

func splitSeries(k string) (name, labels string) {
	if i := strings.IndexByte(k, '{'); i >= 0 {
		return k[:i], k[i+1 : len(k)-1]
	}
	return k, ""
}

// Write renders every metric in the Prometheus text exposition format.
func (r *Registry) Write(w io.Writer) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	var b strings.Builder
	typed := map[string]bool{}
	header := func(name, typ string) {
		if typed[name] {
			return
		}
		typed[name] = true
		fmt.Fprintf(&b, "# HELP %s %s\n# TYPE %s %s\n", name, r.help[name], name, typ)
	}
	for _, k := range sortedKeys(r.counters) {
		name, _ := splitSeries(k)
		header(name, "counter")
		fmt.Fprintf(&b, "%s %d\n", k, r.counters[k].Value())
	}
	for _, k := range sortedKeys(r.gauges) {
		header(k, "gauge")
		fmt.Fprintf(&b, "%s %s\n", k, formatFloat(r.gauges[k]()))
	}
	for _, k := range sortedKeys(r.histograms) {
		name, labels := splitSeries(k)
		header(name, "histogram")
		h := r.histograms[k]
		sep := ""
		if labels != "" {
			sep = ","
		}
		var cum uint64
		for i, bound := range h.bounds {
			cum += h.counts[i].Load()
			fmt.Fprintf(&b, "%s_bucket{%s%sle=\"%s\"} %d\n", name, labels, sep, formatFloat(bound), cum)
		}
		cum += h.counts[len(h.bounds)].Load()
		fmt.Fprintf(&b, "%s_bucket{%s%sle=\"+Inf\"} %d\n", name, labels, sep, cum)
		fmt.Fprintf(&b, "%s_sum%s %s\n", name, braces(labels), formatFloat(float64(h.sumNs.Load())/1e9))
		fmt.Fprintf(&b, "%s_count%s %d\n", name, braces(labels), h.total.Load())
	}
	_, err := io.WriteString(w, b.String())
	return err
}

func braces(labels string) string {
	if labels == "" {
		return ""
	}
	return "{" + labels + "}"
}

func formatFloat(f float64) string {
	if math.IsInf(f, 1) {
		return "+Inf"
	}
	return strings.TrimRight(strings.TrimRight(fmt.Sprintf("%.6f", f), "0"), ".")
}

func sortedKeys[V any](m map[string]V) []string {
	ks := make([]string, 0, len(m))
	for k := range m {
		ks = append(ks, k)
	}
	sort.Strings(ks)
	return ks
}
