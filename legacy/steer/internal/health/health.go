// Package health runs active HTTP health checks against endpoints.
//
// Each endpoint is probed on its own schedule: immediately at start, then
// every interval plus a random jitter so that probes from many proxies do not
// arrive in lockstep. A probe passes on any 2xx response within the timeout.
// An endpoint becomes unhealthy after unhealthy_threshold failures in a row
// and healthy again after healthy_threshold passes in a row, so one lost
// probe does not flap it.
package health

import (
	"context"
	"fmt"
	"io"
	"math/rand/v2"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/laiwf94/steer/internal/clock"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/endpoint"
)

// ChangeFunc is told when an endpoint's verdict flips.
type ChangeFunc func(ep *endpoint.Endpoint, healthy bool, reason string)

// Checker probes a set of endpoints.
type Checker struct {
	cfg      config.HealthCheck
	eps      []*endpoint.Endpoint
	clk      clock.Clock
	client   *http.Client
	random   func() float64
	onChange ChangeFunc
}

// New returns a checker. onChange may be nil.
func New(cfg config.HealthCheck, eps []*endpoint.Endpoint, clk clock.Clock, onChange ChangeFunc) *Checker {
	if clk == nil {
		clk = clock.Real{}
	}
	if onChange == nil {
		onChange = func(*endpoint.Endpoint, bool, string) {}
	}
	tr := &http.Transport{
		DialContext:            (&net.Dialer{Timeout: cfg.Timeout}).DialContext,
		DisableKeepAlives:      true, // every probe proves a new connection can be made
		ResponseHeaderTimeout:  cfg.Timeout,
		MaxResponseHeaderBytes: 16 << 10,
	}
	return &Checker{
		cfg: cfg, eps: eps, clk: clk, random: rand.Float64, onChange: onChange,
		client: &http.Client{
			Transport: tr,
			Timeout:   cfg.Timeout,
			CheckRedirect: func(*http.Request, []*http.Request) error {
				return http.ErrUseLastResponse
			},
		},
	}
}

// Run probes every endpoint until ctx ends, then returns once all probe
// loops have stopped.
func (c *Checker) Run(ctx context.Context) {
	var wg sync.WaitGroup
	for _, ep := range c.eps {
		wg.Go(func() { c.loop(ctx, ep) })
	}
	wg.Wait()
	c.client.CloseIdleConnections()
}

func (c *Checker) loop(ctx context.Context, ep *endpoint.Endpoint) {
	for {
		c.CheckOnce(ctx, ep)
		wait := c.cfg.Interval + time.Duration(c.random()*float64(c.cfg.Jitter))
		select {
		case <-ctx.Done():
			return
		case <-c.clk.After(wait):
		}
	}
}

// CheckOnce probes ep once and applies the result.
func (c *Checker) CheckOnce(ctx context.Context, ep *endpoint.Endpoint) {
	err := c.probe(ctx, ep)
	if ctx.Err() != nil {
		return // shutting down: a canceled probe says nothing about the endpoint
	}
	reason := ""
	if err != nil {
		reason = err.Error()
	}
	healthy, changed := ep.RecordProbe(err == nil, reason, c.cfg.HealthyThreshold, c.cfg.UnhealthyThreshold)
	if changed {
		c.onChange(ep, healthy, reason)
	}
}

func (c *Checker) probe(ctx context.Context, ep *endpoint.Endpoint) error {
	ctx, cancel := context.WithTimeout(ctx, c.cfg.Timeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, "http://"+ep.Address+c.cfg.Path, nil)
	if err != nil {
		return err
	}
	if c.cfg.Host != "" {
		req.Host = c.cfg.Host
	}
	req.Header.Set("User-Agent", "steer-health-check")
	resp, err := c.client.Do(req)
	if err != nil {
		return fmt.Errorf("probe failed: %w", unwrapURLError(err))
	}
	_, _ = io.CopyN(io.Discard, resp.Body, 4<<10)
	resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return fmt.Errorf("probe returned status %d", resp.StatusCode)
	}
	return nil
}

// unwrapURLError drops the *url.Error wrapper, whose message repeats the URL.
func unwrapURLError(err error) error {
	type unwrapper interface{ Unwrap() error }
	if u, ok := err.(unwrapper); ok && u.Unwrap() != nil {
		return u.Unwrap()
	}
	return err
}
