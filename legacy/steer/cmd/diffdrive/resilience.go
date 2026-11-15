package main

// The outlier query runs one failure trace through steer's outlier.Detector:
//
//	{"op":"outlier","hosts":4,"outlier":{"consecutive_gateway_failures":5,
//	 "interval":1000000000,"base_ejection_time":30000000000,
//	 "max_ejection_time":300000000000,"max_ejection_percent":50,
//	 "success_rate_minimum_hosts":3,"success_rate_request_volume":20,
//	 "success_rate_stdev_factor":1.9},
//	 "script":"f 0 1 1\nk\ns"}
//	{"ok":true,"lines":["ok","ok","state 0/- 0/- 0/- 0/-"]}
//
// The cluster has hosts endpoints, 10.0.0.1:80 upwards. Durations are
// nanoseconds. The detector reads a fake clock that starts at pickEpoch, and
// times in answers are nanoseconds since then. One line per operation:
//
//	f I G F   endpoint I reports an attempt; G and F (0 or 1) say whether it
//	          was a gateway failure and a failure
//	k         Tick: decay multipliers, judge success rates
//	t NS      the clock advances NS nanoseconds
//	c I       ClearEjection on endpoint I
//	s         the state: state M/U ... per endpoint, the ejection multiplier
//	          M and the end U of its current or last ejection, - for none
//
// f and k answer the ejections they caused, in order, as eject I REASON
// UNTIL joined by "; ", or ok when there were none. t and c answer ok.
//
// The retry query runs one request trace through steer's retry decisions:
// Idempotent, Retryable and GatewayFailure from internal/proxy, a
// retry.Budget on the fake clock and retry.Backoff:
//
//	{"op":"retry","hosts":3,"seed":9,"retry":{"max_retries":2,
//	 "retry_on":[502,503,504],"backoff_base":25000000,"backoff_max":250000000,
//	 "ratio":0.2,"min_per_second":10,"window":10000000000},
//	 "script":"q GET 0 1 503 c\nt 1000\nb"}
//	{"ok":true,"lines":["g r1234567,g r2345678,o d","ok","balance 98.2"]}
//
//	q METHOD K B OUT...  a request: METHOD as sent, K 1 for a non-empty
//	                     Idempotency-Key, B 1 for a replayable body, and the
//	                     outcome of each attempt in order: s (a response
//	                     below 500), a 5xx status, c (connect error), t
//	                     (timeout) or r (reset); attempts past the list
//	                     succeed
//	t NS                 the clock advances NS nanoseconds
//	b                    the budget's Balance, as strconv's shortest 'f'
//
// A request deposits into the budget, then makes attempts as RoundTrip
// does. Each attempt answers its class (o success, f failure, g gateway
// failure) and how it ended: d (not retryable), n (retryable, but every one
// of the hosts endpoints was tried), x (the budget refused) or rNS (retried
// after a backoff of NS nanoseconds, the jitter drawn from a SplitMix64
// seeded with seed). Backoffs are reported but do not move the clock. A
// request answers its attempts joined by ","; t answers ok.
//
// A malformed line is a bug in the caller and stops the driver with an
// error.

import (
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
	"github.com/laiwf94/steer/internal/outlier"
	"github.com/laiwf94/steer/internal/proxy"
	"github.com/laiwf94/steer/internal/retry"
)

type outlierSpec struct {
	ConsecutiveGatewayFailures int     `json:"consecutive_gateway_failures"`
	Interval                   int64   `json:"interval"`
	BaseEjectionTime           int64   `json:"base_ejection_time"`
	MaxEjectionTime            int64   `json:"max_ejection_time"`
	MaxEjectionPercent         int     `json:"max_ejection_percent"`
	SuccessRateMinimumHosts    int     `json:"success_rate_minimum_hosts"`
	SuccessRateRequestVolume   int     `json:"success_rate_request_volume"`
	SuccessRateStdevFactor     float64 `json:"success_rate_stdev_factor"`
}

type retrySpec struct {
	MaxRetries   int     `json:"max_retries"`
	RetryOn      []int   `json:"retry_on"`
	BackoffBase  int64   `json:"backoff_base"`
	BackoffMax   int64   `json:"backoff_max"`
	Ratio        float64 `json:"ratio"`
	MinPerSecond int     `json:"min_per_second"`
	Window       int64   `json:"window"`
}

// hostAddress is the address of endpoint i in outlier and retry queries.
func hostAddress(i int) string {
	return fmt.Sprintf("10.0.%d.%d:80", i/250, i%250+1)
}

// since writes t as nanoseconds since pickEpoch.
func since(t time.Time) string {
	return strconv.FormatInt(t.Sub(pickEpoch).Nanoseconds(), 10)
}

func outliers(q *query) (answer, error) {
	s := q.Outlier
	if s == nil || q.Hosts < 1 || s.BaseEjectionTime <= 0 || s.MaxEjectionTime < s.BaseEjectionTime {
		return answer{}, fmt.Errorf("outlier query needs hosts and a valid detector")
	}
	eps := make([]*endpoint.Endpoint, q.Hosts)
	index := make(map[*endpoint.Endpoint]int, q.Hosts)
	for i := range eps {
		eps[i] = endpoint.New(hostAddress(i), 1, 0)
		index[eps[i]] = i
	}
	clk := clock.NewFake(pickEpoch)
	var ejections []string
	cfg := config.OutlierDetection{
		ConsecutiveGatewayFailures: s.ConsecutiveGatewayFailures,
		Interval:                   time.Duration(s.Interval),
		BaseEjectionTime:           time.Duration(s.BaseEjectionTime),
		MaxEjectionTime:            time.Duration(s.MaxEjectionTime),
		MaxEjectionPercent:         s.MaxEjectionPercent,
		SuccessRateMinimumHosts:    s.SuccessRateMinimumHosts,
		SuccessRateRequestVolume:   s.SuccessRateRequestVolume,
		SuccessRateStdevFactor:     s.SuccessRateStdevFactor,
	}
	d := outlier.New(cfg, eps, clk, func(ep *endpoint.Endpoint, reason string, until time.Time) {
		ejections = append(ejections, fmt.Sprintf("eject %d %s %s", index[ep], reason, since(until)))
	})
	ejected := func() string {
		if len(ejections) == 0 {
			return "ok"
		}
		line := strings.Join(ejections, "; ")
		ejections = ejections[:0]
		return line
	}
	ops := strings.Split(q.Script, "\n")
	lines := make([]string, 0, len(ops))
	var b strings.Builder
	for n, op := range ops {
		f := strings.Fields(op)
		ints := numbers(f[min(1, len(f)):])
		ep := len(ints) > 0 && ints[0] >= 0 && ints[0] < int64(len(eps))
		flag := func(i int) bool { return ints[i] == 0 || ints[i] == 1 }
		line := "ok"
		switch {
		case len(f) == 4 && f[0] == "f" && ep && flag(1) && flag(2):
			d.Report(eps[ints[0]], ints[1] == 1, ints[2] == 1)
			line = ejected()
		case len(f) == 1 && f[0] == "k":
			d.Tick()
			line = ejected()
		case len(f) == 2 && f[0] == "t" && ints[0] >= 0:
			clk.Advance(time.Duration(ints[0]))
		case len(f) == 2 && f[0] == "c" && ep:
			eps[ints[0]].ClearEjection()
		case len(f) == 1 && f[0] == "s":
			b.Reset()
			b.WriteString("state")
			for _, e := range eps {
				fmt.Fprintf(&b, " %d/", e.TimesEjected())
				if u := e.EjectedUntil(); u.IsZero() {
					b.WriteByte('-')
				} else {
					b.WriteString(since(u))
				}
			}
			line = b.String()
		default:
			return answer{}, fmt.Errorf("op %d %q: malformed", n+1, op)
		}
		lines = append(lines, line)
	}
	return answer{OK: true, Lines: lines}, nil
}

// outcomes maps the attempt letters of a q line to RoundTrip's results.
var outcomes = map[string]int{
	"s": proxy.ResultSuccess,
	"c": proxy.ResultConnectError,
	"t": proxy.ResultTimeout,
	"r": proxy.ResultReset,
}

func retries(q *query) (answer, error) {
	s := q.Retry
	if s == nil || q.Hosts < 1 || s.Window < int64(time.Second) {
		return answer{}, fmt.Errorf("retry query needs hosts and a valid policy")
	}
	for _, code := range s.RetryOn {
		if code < 500 || code > 599 {
			return answer{}, fmt.Errorf("retry_on %d is not a 5xx status", code)
		}
	}
	pol := &config.Retry{
		MaxRetries:  s.MaxRetries,
		RetryOn:     s.RetryOn,
		BackoffBase: time.Duration(s.BackoffBase),
		BackoffMax:  time.Duration(s.BackoffMax),
		Budget:      config.Budget{Ratio: s.Ratio, MinPerSecond: s.MinPerSecond, Window: time.Duration(s.Window)},
	}
	retryOn := proxy.RetryOnSet(pol)
	clk := clock.NewFake(pickEpoch)
	budget := retry.NewBudget(s.Ratio, s.MinPerSecond, time.Duration(s.Window), clk)
	jitter := newSplitMix(q.Seed)
	ops := strings.Split(q.Script, "\n")
	lines := make([]string, 0, len(ops))
	var b strings.Builder
	for n, op := range ops {
		f := strings.Fields(op)
		bad := func() (answer, error) { return answer{}, fmt.Errorf("op %d %q: malformed", n+1, op) }
		line := "ok"
		switch {
		case len(f) >= 4 && f[0] == "q" && (f[2] == "0" || f[2] == "1") && (f[3] == "0" || f[3] == "1"):
			req := &http.Request{Method: f[1], Header: http.Header{}}
			if f[2] == "1" {
				req.Header.Set("Idempotency-Key", "k")
			}
			replayable := f[3] == "1"
			outs := f[4:]
			budget.Deposit()
			safe := proxy.Idempotent(req)
			b.Reset()
			for attempt := 0; ; attempt++ {
				result, status := proxy.ResultSuccess, 200
				if attempt < len(outs) {
					if r, ok := outcomes[outs[attempt]]; ok {
						result, status = r, 0
						if r == proxy.ResultSuccess {
							status = 200
						}
					} else if code, err := strconv.Atoi(outs[attempt]); err == nil && code >= 500 && code <= 599 {
						result, status = proxy.ResultHTTP5xx, code
					} else {
						return bad()
					}
				}
				if attempt > 0 {
					b.WriteByte(',')
				}
				switch {
				case proxy.GatewayFailure(result, status):
					b.WriteString("g ")
				case result != proxy.ResultSuccess:
					b.WriteString("f ")
				default:
					b.WriteString("o ")
				}
				if !proxy.Retryable(pol, &retryOn, attempt, replayable, safe, result, status) {
					b.WriteByte('d')
					break
				}
				// steer's balancer never repeats an endpoint within a
				// request, and every endpoint here is available.
				if attempt+1 >= q.Hosts {
					b.WriteByte('n')
					break
				}
				if !budget.TryWithdraw() {
					b.WriteByte('x')
					break
				}
				wait := retry.Backoff(attempt, pol.BackoffBase, pol.BackoffMax, jitter.Float64)
				fmt.Fprintf(&b, "r%d", wait.Nanoseconds())
			}
			line = b.String()
		case len(f) == 2 && f[0] == "t":
			ns, err := strconv.ParseInt(f[1], 10, 64)
			if err != nil || ns < 0 {
				return bad()
			}
			clk.Advance(time.Duration(ns))
		case len(f) == 1 && f[0] == "b":
			line = "balance " + strconv.FormatFloat(budget.Balance(), 'f', -1, 64)
		default:
			return bad()
		}
		lines = append(lines, line)
	}
	return answer{OK: true, Lines: lines}, nil
}
