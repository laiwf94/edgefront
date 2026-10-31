package proxy

import (
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/metrics"
)

// The proxy-overhead benchmark: the same small GET straight to an upstream
// and through steer (route match, LB pick, retry bookkeeping, header
// rewrite, access log to io.Discard), both over loopback with keep-alive.

func benchUpstream(b *testing.B) *httptest.Server {
	b.Helper()
	body := []byte(`{"ok":true}`)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write(body)
	}))
	b.Cleanup(srv.Close)
	return srv
}

// benchGet measures throughput with parallel clients, or the latency of one
// request at a time when serial is set.
func benchGet(b *testing.B, url string, serial bool) {
	client := &http.Client{Transport: &http.Transport{MaxIdleConnsPerHost: 512, MaxIdleConns: 512}}
	one := func() bool {
		resp, err := client.Get(url)
		if err != nil {
			b.Error(err)
			return false
		}
		io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
		if resp.StatusCode != 200 {
			b.Errorf("status %d", resp.StatusCode)
			return false
		}
		return true
	}
	b.ReportAllocs()
	b.ResetTimer()
	if serial {
		for b.Loop() {
			if !one() {
				return
			}
		}
		return
	}
	b.RunParallel(func(pb *testing.PB) {
		for pb.Next() {
			if !one() {
				return
			}
		}
	})
}

func BenchmarkDirect(b *testing.B)       { benchGet(b, benchUpstream(b).URL+"/bench", false) }
func BenchmarkDirectSerial(b *testing.B) { benchGet(b, benchUpstream(b).URL+"/bench", true) }

func benchProxy(b *testing.B, extra string, serial bool) {
	up := benchUpstream(b)
	cfg, err := parseBench(oneCluster(extra, up.Listener.Addr().String()))
	if err != nil {
		b.Fatal(err)
	}
	p, err := New(cfg, Options{Logger: slog.New(slog.NewJSONHandler(io.Discard, nil)), Metrics: metrics.New()})
	if err != nil {
		b.Fatal(err)
	}
	b.Cleanup(p.Close)
	srv := httptest.NewServer(p)
	b.Cleanup(srv.Close)
	benchGet(b, srv.URL+"/bench", serial)
}

const allFeatures = `"policy":"least_loaded","retry":{"per_try_timeout":"1s"},"outlier_detection":{}`

func BenchmarkThroughSteer(b *testing.B)                  { benchProxy(b, "", false) }
func BenchmarkThroughSteerSerial(b *testing.B)            { benchProxy(b, "", true) }
func BenchmarkThroughSteerAllFeatures(b *testing.B)       { benchProxy(b, allFeatures, false) }
func BenchmarkThroughSteerAllFeaturesSerial(b *testing.B) { benchProxy(b, allFeatures, true) }

func parseBench(js string) (*config.Config, error) { return config.Parse([]byte(js)) }
