package proxy

import (
	"bytes"
	"context"
	"errors"
	"io"
	"math/rand/v2"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/laiwf94/steer/internal/balancer"
	"github.com/laiwf94/steer/internal/endpoint"
	"github.com/laiwf94/steer/internal/retry"
)

// failurePenalty is the least latency recorded for a failed attempt. Without
// it a host that refuses connections or returns 503 instantly would look like
// the fastest host to the least-loaded policy and attract more traffic.
const failurePenalty = time.Second

var (
	errNoHealthyUpstream = errors.New("no healthy upstream")
	errPerTryTimeout     = errors.New("per-try timeout")
)

// upstreamError is the final failure of a request that got no response.
// result is the attempt result, or -1 when reading the client's body failed.
type upstreamError struct {
	result int
	err    error
}

func (e *upstreamError) Error() string { return e.err.Error() }
func (e *upstreamError) Unwrap() error { return e.err }

// roundTripper sends each request to the cluster chosen by ServeHTTP,
// retrying on another endpoint when that is safe and the budget allows.
type roundTripper struct{ s *Server }

func idempotent(r *http.Request) bool {
	switch r.Method {
	case http.MethodGet, http.MethodHead, http.MethodOptions, http.MethodTrace, http.MethodPut, http.MethodDelete:
		return true
	}
	return r.Header.Get("Idempotency-Key") != ""
}

// RoundTrip implements http.RoundTripper.
func (t roundTripper) RoundTrip(req *http.Request) (*http.Response, error) {
	st := stateFrom(req.Context())
	c := st.cluster
	pol := c.cfg.Retry
	ctx := req.Context()

	// Buffer a small body so it can be replayed. A larger body streams to
	// the first endpoint only: it can't be retried.
	var buf []byte
	body := req.Body
	replayable := body == nil || body == http.NoBody
	if !replayable && pol != nil && pol.MaxRetries > 0 && req.ContentLength <= pol.MaxBodyBytes {
		b, err := io.ReadAll(io.LimitReader(body, pol.MaxBodyBytes+1))
		if err != nil {
			if ctx.Err() != nil {
				return nil, ctx.Err()
			}
			return nil, &upstreamError{result: -1, err: err}
		}
		if int64(len(b)) <= pol.MaxBodyBytes {
			buf, replayable = b, true
		} else {
			body = readCloser{io.MultiReader(bytes.NewReader(b), body), body}
		}
	}

	var key uint64
	hasKey := false
	if h := c.cfg.HashHeader; h != "" {
		if v := req.Header.Get(h); v != "" {
			key, hasKey = balancer.HashKey(v), true
		}
	}
	if c.budget != nil {
		c.budget.Deposit()
	}
	safe := idempotent(req)

	var tried []int
	idx := c.lb.Pick(key, hasKey, tried)
	if idx < 0 {
		return nil, errNoHealthyUpstream
	}
	for attempt := 0; ; attempt++ {
		ep := c.eps[idx]
		tried = append(tried, idx)
		st.tried = append(st.tried, ep.Address)
		st.attempts++

		tryCtx, cancel := context.WithCancelCause(ctx)
		out := req.Clone(tryCtx)
		out.URL.Host = ep.Address
		if buf != nil {
			out.Body = io.NopCloser(bytes.NewReader(buf))
			out.ContentLength = int64(len(buf))
			out.GetBody = func() (io.ReadCloser, error) { return io.NopCloser(bytes.NewReader(buf)), nil }
		} else {
			out.Body = body
		}

		ep.Acquire()
		began := time.Now()
		var timer *time.Timer
		if pol != nil && pol.PerTryTimeout > 0 {
			timer = time.AfterFunc(pol.PerTryTimeout, func() { cancel(errPerTryTimeout) })
		}
		resp, err := c.transport.RoundTrip(out)
		if timer != nil && !timer.Stop() && err == nil {
			// The timeout fired as the headers arrived; the attempt's context
			// is gone, so the body is unusable.
			resp.Body.Close()
			resp, err = nil, errPerTryTimeout
		}
		elapsed := time.Since(began)

		if err != nil && ctx.Err() != nil {
			// The client left or the overall timeout passed: not the
			// endpoint's fault, and nothing left to retry for.
			ep.Release()
			cancel(nil)
			st.results = append(st.results, "canceled")
			return nil, ctx.Err()
		}
		result := classify(resp, err, tryCtx)
		st.results = append(st.results, resultNames[result])
		c.attempts[idx][result].Inc()
		gateway := result != resultSuccess && result != resultHTTP5xx || resp != nil && isGatewayStatus(resp.StatusCode)
		if c.detector != nil {
			c.detector.Report(ep, gateway, result != resultSuccess)
		}
		if result == resultSuccess {
			ep.ObserveLatency(elapsed, t.s.clk.Now())
		} else {
			ep.ObserveLatency(max(elapsed, failurePenalty), t.s.clk.Now())
		}

		retryable := false
		if pol != nil && attempt < pol.MaxRetries && replayable {
			switch result {
			case resultConnectError:
				retryable = true // the request never reached the upstream
			case resultTimeout, resultReset:
				retryable = safe
			case resultHTTP5xx:
				retryable = safe && c.retryOn[resp.StatusCode-500]
			}
		}
		next := -1
		if retryable {
			next = c.lb.Pick(key, hasKey, tried)
		}
		if next >= 0 && !c.budget.TryWithdraw() {
			c.budgetExhausted.Inc()
			next = -1
		}
		if next >= 0 && !sleep(ctx, retry.Backoff(attempt, pol.BackoffBase, pol.BackoffMax, rand.Float64)) {
			next = -1
		}
		if next < 0 {
			if resp != nil && ctx.Err() == nil {
				resp.Body = trackBody(resp, ep, cancel)
				st.body = resp.Body
				return resp, nil
			}
			if resp != nil {
				resp.Body.Close() // the deadline passed while backing off; the body is unreadable
			}
			ep.Release()
			cancel(nil)
			if ctx.Err() != nil {
				return nil, ctx.Err()
			}
			return nil, &upstreamError{result: result, err: err}
		}
		if resp != nil {
			// Drain a little so the connection can be reused.
			_, _ = io.CopyN(io.Discard, resp.Body, 4<<10)
			resp.Body.Close()
		}
		ep.Release()
		cancel(nil)
		c.retries.Inc()
		idx = next
	}
}

func isGatewayStatus(code int) bool {
	return code == http.StatusBadGateway || code == http.StatusServiceUnavailable || code == http.StatusGatewayTimeout
}

func classify(resp *http.Response, err error, tryCtx context.Context) int {
	if err == nil {
		if resp.StatusCode >= 500 && resp.StatusCode <= 599 {
			return resultHTTP5xx
		}
		return resultSuccess
	}
	if errors.Is(err, errPerTryTimeout) || errors.Is(context.Cause(tryCtx), errPerTryTimeout) {
		return resultTimeout
	}
	var op *net.OpError
	if errors.As(err, &op) && op.Op == "dial" {
		return resultConnectError
	}
	var ne net.Error
	if errors.As(err, &ne) && ne.Timeout() {
		return resultTimeout
	}
	return resultReset
}

// sleep waits for d or until ctx ends, reporting whether the full wait passed.
func sleep(ctx context.Context, d time.Duration) bool {
	if d <= 0 {
		return ctx.Err() == nil
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-t.C:
		return true
	case <-ctx.Done():
		return false
	}
}

type readCloser struct {
	io.Reader
	io.Closer
}

// trackBody releases the endpoint's in-flight slot and the attempt's context
// once ReverseProxy is done with the response body. A 101 response body is
// the upgraded connection and must stay writable.
func trackBody(resp *http.Response, ep *endpoint.Endpoint, cancel context.CancelCauseFunc) io.ReadCloser {
	var once sync.Once
	done := func() { once.Do(func() { ep.Release(); cancel(nil) }) }
	if rwc, ok := resp.Body.(io.ReadWriteCloser); ok && resp.StatusCode == http.StatusSwitchingProtocols {
		return &upgradeBody{ReadWriteCloser: rwc, done: done}
	}
	return &trackedBody{ReadCloser: resp.Body, done: done}
}

type trackedBody struct {
	io.ReadCloser
	done func()
}

func (b *trackedBody) Close() error {
	err := b.ReadCloser.Close()
	b.done()
	return err
}

type upgradeBody struct {
	io.ReadWriteCloser
	done func()
}

func (b *upgradeBody) Close() error {
	err := b.ReadWriteCloser.Close()
	b.done()
	return err
}
