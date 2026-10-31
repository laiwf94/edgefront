package proxy

import (
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/fakeupstream"
)

// resilient is the configuration the chaos test and examples/steer.json use,
// with intervals shortened for test speed.
const resilient = `"policy":"least_loaded",
	"timeout":"2s",
	"health_check":{"path":"/healthz","interval":"100ms","jitter":"20ms","timeout":"50ms",
	                "healthy_threshold":2,"unhealthy_threshold":2},
	"outlier_detection":{"consecutive_gateway_failures":5,"interval":"200ms","base_ejection_time":"5s",
	                     "max_ejection_percent":50,"success_rate_minimum_hosts":3,"success_rate_request_volume":20},
	"retry":{"max_retries":2,"per_try_timeout":"100ms","backoff_base":"5ms","backoff_max":"20ms",
	         "budget":{"ratio":0.2,"min_per_second":10,"window":"10s"}}`

// plain has none of the resilience features, only a request timeout.
const plain = `"policy":"round_robin","timeout":"200ms"`

type chaosResult struct {
	ok, total int64
	byStatus  map[int]int64
}

func runChaos(t *testing.T, clusterExtra string, requests, concurrency int) chaosResult {
	t.Helper()
	var addrs []string
	ups := make([]*fakeupstream.Server, 5)
	for i := range ups {
		ups[i] = fakeupstream.New(fmt.Sprint("u", i))
		srv := httptest.NewServer(ups[i].Handler())
		t.Cleanup(srv.Close)
		t.Cleanup(ups[i].Close)
		addrs = append(addrs, addr(srv))
	}
	ups[0].Set(fakeupstream.Settings{Blackhole: true})
	ups[1].Set(fakeupstream.Settings{ErrorRate: 0.5})
	h := newHarness(t, oneCluster(clusterExtra, addrs...))

	client := &http.Client{Timeout: 5 * time.Second, Transport: &http.Transport{MaxIdleConnsPerHost: concurrency}}
	var ok, total atomic.Int64
	var mu sync.Mutex
	byStatus := map[int]int64{}
	work := make(chan struct{})
	var wg sync.WaitGroup
	for range concurrency {
		wg.Go(func() {
			for range work {
				total.Add(1)
				resp, err := client.Get(h.proxy + "/api")
				code := 0
				if err == nil {
					io.Copy(io.Discard, resp.Body)
					resp.Body.Close()
					code = resp.StatusCode
				}
				if code == 200 {
					ok.Add(1)
				}
				mu.Lock()
				byStatus[code]++
				mu.Unlock()
			}
		})
	}
	for range requests {
		work <- struct{}{}
	}
	close(work)
	wg.Wait()
	st := h.p.State().Clusters[0]
	var lines []string
	for _, ep := range st.Endpoints {
		lines = append(lines, fmt.Sprintf("%s healthy=%v ejected=%v times=%d", ep.Address, ep.Healthy, ep.Ejected, ep.TimesEjected))
	}
	t.Logf("endpoints:\n  %s", strings.Join(lines, "\n  "))
	return chaosResult{ok.Load(), total.Load(), byStatus}
}

// One blackholed and one 50%-error upstream among five: with health checks,
// outlier detection and budgeted retries, clients must see at least 99.5%
// success. The same faults without them fail far more often, which shows the
// result comes from the features and not from an easy workload.
func TestChaos(t *testing.T) {
	if testing.Short() {
		t.Skip("chaos test takes a few seconds")
	}
	r := runChaos(t, resilient, 4000, 32)
	rate := float64(r.ok) / float64(r.total)
	t.Logf("resilient: %d/%d ok (%.3f%%), statuses %v", r.ok, r.total, 100*rate, r.byStatus)
	if rate < 0.995 {
		t.Fatalf("success rate %.3f%% < 99.5%%", 100*rate)
	}

	r = runChaos(t, plain, 1000, 32)
	base := float64(r.ok) / float64(r.total)
	t.Logf("plain: %d/%d ok (%.3f%%), statuses %v", r.ok, r.total, 100*base, r.byStatus)
	if base > 0.9 {
		t.Fatalf("without resilience features success was %.1f%%; the chaos is too weak to mean anything", 100*base)
	}
}
