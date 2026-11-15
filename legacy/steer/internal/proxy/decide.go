package proxy

import (
	"net/http"

	"github.com/laiwf94/steer/internal/config"
)

// The per-attempt decisions RoundTrip makes, kept apart from the HTTP
// plumbing so that cmd/diffdrive can ask them directly and edgefront's
// Rust port can be diffed against them.

// Attempt results, as RoundTrip classifies them; ResultNames gives their
// metric labels.
const (
	ResultSuccess      = resultSuccess
	ResultHTTP5xx      = resultHTTP5xx
	ResultConnectError = resultConnectError
	ResultTimeout      = resultTimeout
	ResultReset        = resultReset
)

// ResultNames are the result labels of steer_upstream_attempts_total.
var ResultNames = resultNames

// Idempotent reports whether a request may be sent twice: an idempotent
// method, or a non-empty Idempotency-Key header.
func Idempotent(r *http.Request) bool {
	switch r.Method {
	case http.MethodGet, http.MethodHead, http.MethodOptions, http.MethodTrace, http.MethodPut, http.MethodDelete:
		return true
	}
	return r.Header.Get("Idempotency-Key") != ""
}

// RetryOnSet returns the statuses pol retries, indexed by status - 500.
func RetryOnSet(pol *config.Retry) [100]bool {
	var set [100]bool
	for _, code := range pol.RetryOn {
		set[code-500] = true
	}
	return set
}

// Retryable reports whether attempt number attempt (0 for the first try),
// which ended with result and, when a response arrived, status, may be
// retried under pol (nil for no retries). replayable says whether the body
// can be sent again and safe whether the request is Idempotent. A retry
// still needs another endpoint and room in the budget.
func Retryable(pol *config.Retry, retryOn *[100]bool, attempt int, replayable, safe bool, result, status int) bool {
	if pol == nil || attempt >= pol.MaxRetries || !replayable {
		return false
	}
	switch result {
	case resultConnectError:
		return true // the request never reached the upstream
	case resultTimeout, resultReset:
		return safe
	case resultHTTP5xx:
		return safe && retryOn[status-500]
	}
	return false
}

// GatewayFailure reports whether an attempt counts as a gateway failure for
// outlier detection: no response at all, or a 502, 503 or 504. status is 0
// when no response arrived.
func GatewayFailure(result, status int) bool {
	return result != resultSuccess && result != resultHTTP5xx || isGatewayStatus(status)
}
