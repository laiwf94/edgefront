// Command loadgen sends HTTP requests at a constant rate and reports the
// client-side success rate and latency percentiles.
//
// It is open-loop: request i is scheduled at start + i/rate whether or not
// earlier requests have finished, and latency is measured from that scheduled
// time. A closed-loop tester (send, wait, send) slows down when the server
// does and hides exactly the stalls worth seeing ("coordinated omission").
//
// Flags:
//
//	-url           target URL (required)
//	-rate          requests per second (default 200)
//	-duration      how long to send (default 10s)
//	-timeout       per-request timeout; a timeout counts as a failure (default 2s)
//	-method        HTTP method (default GET)
//	-H             extra header "Name: value", repeatable
//	-max-inflight  requests outstanding before new ones are counted as dropped (default 10000)
//	-format        text, row (a Markdown table row) or json (default text)
//	-label         first column of -format row
//
// Success means a 2xx response. Percentiles cover every request, failures
// included, at the time they failed.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"net/http"
	"net/textproto"
	"net/url"
	"os"
	"os/signal"
	"sort"
	"strings"
	"sync"
	"syscall"
	"time"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, os.Args[1:], os.Stdout, os.Stderr); err != nil {
		fmt.Fprintln(os.Stderr, "loadgen:", err)
		os.Exit(2)
	}
}

// maxRequests bounds one run, since every result is kept for percentiles.
const maxRequests = 10_000_000

type headerFlag []([2]string)

func (h *headerFlag) String() string { return fmt.Sprint(*h) }

func (h *headerFlag) Set(v string) error {
	name, value, err := parseHeader(v)
	if err != nil {
		return err
	}
	*h = append(*h, [2]string{name, value})
	return nil
}

// parseHeader splits "Name: value", rejecting invalid names and values with
// control characters.
func parseHeader(s string) (string, string, error) {
	name, value, ok := strings.Cut(s, ":")
	name, value = strings.TrimSpace(name), strings.TrimSpace(value)
	if !ok || name == "" || len(s) > 8<<10 {
		return "", "", fmt.Errorf("header %q must look like \"Name: value\"", s)
	}
	for i := 0; i < len(name); i++ {
		c := name[i]
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || strings.IndexByte("!#$%&'*+-.^_`|~", c) >= 0) {
			return "", "", fmt.Errorf("header name %q is not a valid token", name)
		}
	}
	for i := 0; i < len(value); i++ {
		if c := value[i]; c < ' ' && c != '\t' || c == 0x7f {
			return "", "", fmt.Errorf("header value for %q contains control characters", name)
		}
	}
	return textproto.CanonicalMIMEHeaderKey(name), value, nil
}

type options struct {
	url         string
	rate        float64
	duration    time.Duration
	timeout     time.Duration
	method      string
	headers     headerFlag
	maxInflight int
	format      string
	label       string
}

func parseFlags(args []string, errOut io.Writer) (options, error) {
	var o options
	fs := flag.NewFlagSet("loadgen", flag.ContinueOnError)
	fs.SetOutput(errOut)
	fs.StringVar(&o.url, "url", "", "target URL (required)")
	fs.Float64Var(&o.rate, "rate", 200, "requests per second")
	fs.DurationVar(&o.duration, "duration", 10*time.Second, "how long to send")
	fs.DurationVar(&o.timeout, "timeout", 2*time.Second, "per-request timeout")
	fs.StringVar(&o.method, "method", http.MethodGet, "HTTP method")
	fs.Var(&o.headers, "H", `extra header "Name: value" (repeatable)`)
	fs.IntVar(&o.maxInflight, "max-inflight", 10000, "outstanding requests before new ones are dropped")
	fs.StringVar(&o.format, "format", "text", "text, row or json")
	fs.StringVar(&o.label, "label", "", "first column for -format row")
	if err := fs.Parse(args); err != nil {
		return o, err
	}
	if fs.NArg() > 0 {
		return o, fmt.Errorf("unexpected arguments: %v", fs.Args())
	}
	u, err := url.Parse(o.url)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return o, errors.New("-url must be an http:// or https:// URL")
	}
	switch {
	case !(o.rate > 0 && o.rate <= 1_000_000):
		return o, errors.New("-rate must be between 0 and 1000000")
	case o.duration <= 0 || o.duration > 24*time.Hour:
		return o, errors.New("-duration must be between 0 and 24h")
	case o.timeout <= 0:
		return o, errors.New("-timeout must be positive")
	case o.maxInflight < 1:
		return o, errors.New("-max-inflight must be at least 1")
	case o.rate*o.duration.Seconds() > maxRequests:
		return o, fmt.Errorf("-rate x -duration must not exceed %d requests", maxRequests)
	case o.format != "text" && o.format != "row" && o.format != "json":
		return o, errors.New("-format must be text, row or json")
	}
	for _, c := range o.method {
		if c < 'A' || c > 'Z' {
			return o, errors.New("-method must be an upper-case HTTP method")
		}
	}
	return o, nil
}

// Report summarises a run.
type Report struct {
	Label      string         `json:"label,omitempty"`
	Requests   int            `json:"requests"`
	OK         int            `json:"ok"`
	Success    float64        `json:"success_percent"`
	Failures   map[string]int `json:"failures"`
	P50Ms      float64        `json:"p50_ms"`
	P99Ms      float64        `json:"p99_ms"`
	P999Ms     float64        `json:"p999_ms"`
	MaxMs      float64        `json:"max_ms"`
	ElapsedSec float64        `json:"elapsed_s"`
}

type sample struct {
	d       time.Duration
	outcome string // "ok" or a failure kind
}

func run(ctx context.Context, args []string, out, errOut io.Writer) error {
	o, err := parseFlags(args, errOut)
	if err != nil {
		return err
	}
	tr := &http.Transport{MaxIdleConns: o.maxInflight, MaxIdleConnsPerHost: o.maxInflight, IdleConnTimeout: 30 * time.Second,
		DisableCompression: true, Proxy: nil}
	defer tr.CloseIdleConnections()
	client := &http.Client{Transport: tr, Timeout: o.timeout,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}

	var (
		mu      sync.Mutex
		samples []sample
		wg      sync.WaitGroup
	)
	record := func(s sample) {
		mu.Lock()
		samples = append(samples, s)
		mu.Unlock()
	}
	sem := make(chan struct{}, o.maxInflight)
	interval := time.Duration(float64(time.Second) / o.rate)
	start := time.Now()
	total := int(o.rate * o.duration.Seconds())
schedule:
	for i := 0; i < total; i++ {
		at := start.Add(time.Duration(i) * interval)
		if wait := time.Until(at); wait > 0 {
			select {
			case <-time.After(wait):
			case <-ctx.Done():
				break schedule
			}
		}
		select {
		case sem <- struct{}{}:
		default:
			record(sample{0, "dropped"})
			continue
		}
		wg.Go(func() {
			defer func() { <-sem }()
			record(send(client, o, at))
		})
	}
	wg.Wait()
	rep := summarise(samples, time.Since(start))
	rep.Label = o.label
	return write(out, o.format, rep)
}

func send(client *http.Client, o options, scheduled time.Time) sample {
	req, err := http.NewRequest(o.method, o.url, nil)
	if err != nil {
		return sample{0, "bad_request"}
	}
	for _, h := range o.headers {
		req.Header.Add(h[0], h[1])
	}
	resp, err := client.Do(req)
	if err != nil {
		var ue *url.Error
		if errors.As(err, &ue) && ue.Timeout() {
			return sample{time.Since(scheduled), "timeout"}
		}
		return sample{time.Since(scheduled), "transport_error"}
	}
	_, _ = io.Copy(io.Discard, resp.Body)
	resp.Body.Close()
	d := time.Since(scheduled)
	if resp.StatusCode >= 200 && resp.StatusCode < 300 {
		return sample{d, "ok"}
	}
	return sample{d, fmt.Sprintf("http_%d", resp.StatusCode)}
}

func summarise(samples []sample, elapsed time.Duration) Report {
	r := Report{Requests: len(samples), Failures: map[string]int{}, ElapsedSec: math.Round(elapsed.Seconds()*100) / 100}
	ds := make([]time.Duration, 0, len(samples))
	for _, s := range samples {
		if s.outcome == "ok" {
			r.OK++
		} else {
			r.Failures[s.outcome]++
		}
		if s.outcome != "dropped" {
			ds = append(ds, s.d)
		}
	}
	if r.Requests > 0 {
		r.Success = math.Round(10000*float64(r.OK)/float64(r.Requests)) / 100
	}
	sort.Slice(ds, func(i, j int) bool { return ds[i] < ds[j] })
	r.P50Ms, r.P99Ms, r.P999Ms = percentile(ds, 0.5), percentile(ds, 0.99), percentile(ds, 0.999)
	if len(ds) > 0 {
		r.MaxMs = ms(ds[len(ds)-1])
	}
	return r
}

// percentile uses the nearest-rank method on sorted durations.
func percentile(sorted []time.Duration, p float64) float64 {
	if len(sorted) == 0 {
		return 0
	}
	rank := int(math.Ceil(p*float64(len(sorted)))) - 1
	return ms(sorted[max(rank, 0)])
}

func ms(d time.Duration) float64 { return math.Round(float64(d.Microseconds())/10) / 100 }

func write(out io.Writer, format string, r Report) error {
	switch format {
	case "json":
		return json.NewEncoder(out).Encode(r)
	case "row":
		_, err := fmt.Fprintf(out, "| %s | %d | %.2f%% | %.1f ms | %.1f ms | %.1f ms |\n", r.Label, r.Requests, r.Success, r.P50Ms, r.P99Ms, r.P999Ms)
		return err
	}
	var fails []string
	for k, v := range r.Failures {
		fails = append(fails, fmt.Sprintf("%s x%d", k, v))
	}
	sort.Strings(fails)
	failures := "none"
	if len(fails) > 0 {
		failures = strings.Join(fails, ", ")
	}
	_, err := fmt.Fprintf(out, "requests %d in %.1fs  success %.2f%%  (%d ok; failures: %s)\nlatency  p50 %.1f ms  p99 %.1f ms  p999 %.1f ms  max %.1f ms\n",
		r.Requests, r.ElapsedSec, r.Success, r.OK, failures, r.P50Ms, r.P99Ms, r.P999Ms, r.MaxMs)
	return err
}
