package proxy

import (
	"net/http"
	"testing"

	"github.com/laiwf94/steer/internal/config"
)

func TestIdempotent(t *testing.T) {
	for _, c := range []struct {
		method, key string
		want        bool
	}{
		{"GET", "", true}, {"HEAD", "", true}, {"OPTIONS", "", true}, {"TRACE", "", true},
		{"PUT", "", true}, {"DELETE", "", true}, {"POST", "", false}, {"PATCH", "", false},
		{"get", "", false}, {"POST", "k", true}, {"CONNECT", "x", true},
	} {
		r := &http.Request{Method: c.method, Header: http.Header{}}
		if c.key != "" {
			r.Header.Set("Idempotency-Key", c.key)
		}
		if got := Idempotent(r); got != c.want {
			t.Errorf("%s key %q: got %v", c.method, c.key, got)
		}
	}
}

func TestRetryableAndGatewayFailure(t *testing.T) {
	pol := &config.Retry{MaxRetries: 2, RetryOn: []int{503, 599}}
	on := RetryOnSet(pol)
	if !on[3] || !on[99] || on[2] {
		t.Fatalf("RetryOnSet: %v", on)
	}
	for _, c := range []struct {
		pol              *config.Retry
		attempt          int
		replayable, safe bool
		result, status   int
		want             bool
	}{
		{nil, 0, true, true, ResultConnectError, 0, false},
		{pol, 2, true, true, ResultConnectError, 0, false},
		{pol, 1, false, true, ResultConnectError, 0, false},
		{pol, 1, true, false, ResultConnectError, 0, true},
		{pol, 0, true, false, ResultTimeout, 0, false},
		{pol, 0, true, true, ResultTimeout, 0, true},
		{pol, 0, true, true, ResultReset, 0, true},
		{pol, 0, true, true, ResultHTTP5xx, 503, true},
		{pol, 0, true, false, ResultHTTP5xx, 503, false},
		{pol, 0, true, true, ResultHTTP5xx, 502, false},
		{pol, 0, true, true, ResultSuccess, 200, false},
	} {
		if got := Retryable(c.pol, &on, c.attempt, c.replayable, c.safe, c.result, c.status); got != c.want {
			t.Errorf("%+v: got %v", c, got)
		}
	}
	for _, c := range []struct {
		result, status int
		want           bool
	}{
		{ResultSuccess, 200, false}, {ResultHTTP5xx, 500, false}, {ResultHTTP5xx, 502, true},
		{ResultHTTP5xx, 503, true}, {ResultHTTP5xx, 504, true}, {ResultHTTP5xx, 505, false},
		{ResultConnectError, 0, true}, {ResultTimeout, 0, true}, {ResultReset, 0, true},
	} {
		if got := GatewayFailure(c.result, c.status); got != c.want {
			t.Errorf("GatewayFailure(%s, %d): got %v", ResultNames[c.result], c.status, got)
		}
	}
}
