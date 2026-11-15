package main

// steer's /metrics, recorded for edgefront's Rust port: the test runs the
// real binary's run() with TLS, two clusters, health checks, outlier
// detection and retries, drives traffic that makes every family appear,
// and scrapes /metrics. The scrape, with the upstreams' random ports
// replaced by fixed ones, is the golden file that
// crates/edgefront-core/tests/metrics_steer.rs diffs the Rust exposition
// against. `go test ./cmd/steer -run MetricsGolden -update` rewrites it;
// without -update the test checks that steer still renders the same
// families, series and help text.

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"flag"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

var updateMetrics = flag.Bool("update", false, "rewrite the metrics golden file")

const metricsGoldenPath = "../../../../crates/edgefront-core/tests/testdata/metrics_steer.prom"

// The fixed addresses the golden file names the upstreams by. The dead
// endpoint is a real fixed address: nothing listens on port 1.
const (
	goldenWebAddr  = "127.0.0.1:19101"
	goldenAPIAddr  = "127.0.0.1:19102"
	goldenDeadAddr = "127.0.0.1:1"
)

// metricsShape is the exposition with every sample's value removed: the
// HELP and TYPE lines and the series names with their labels, in order.
func metricsShape(exposition string) string {
	var b strings.Builder
	for _, line := range strings.Split(strings.TrimSuffix(exposition, "\n"), "\n") {
		if !strings.HasPrefix(line, "#") {
			if i := strings.LastIndexByte(line, ' '); i >= 0 {
				line = line[:i]
			}
		}
		b.WriteString(line)
		b.WriteByte('\n')
	}
	return b.String()
}

func TestMetricsGolden(t *testing.T) {
	dir := t.TempDir()
	caKey, _ := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	caTmpl := &x509.Certificate{SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "test CA"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour), IsCA: true,
		BasicConstraintsValid: true, KeyUsage: x509.KeyUsageCertSign}
	caDER, _ := x509.CreateCertificate(rand.Reader, caTmpl, caTmpl, caKey.Public(), caKey)
	caCert, _ := x509.ParseCertificate(caDER)
	pool := x509.NewCertPool()
	pool.AddCert(caCert)
	writeCert(t, dir, "site", "steer.test", caCert, caKey, 2)

	web := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, "web")
	}))
	defer web.Close()
	// api's endpoint fails its health checks until healthy is set, so the
	// checker reports both transitions, and answers 503 to requests.
	var healthy atomic.Bool
	api := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/healthz" && healthy.Load() {
			return
		}
		w.WriteHeader(http.StatusServiceUnavailable)
	}))
	defer api.Close()
	webAddr, apiAddr := web.Listener.Addr().String(), api.Listener.Addr().String()

	const token = "admin-token-0123456789"
	cfgPath := filepath.Join(dir, "steer.json")
	writeCfg := func(js string) {
		if err := os.WriteFile(cfgPath, []byte(js), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	cfg := fmt.Sprintf(`{"listen":"127.0.0.1:0","admin_listen":"127.0.0.1:0",
		"routes":[{"host":"web.test","cluster":"web"},{"host":"api.test","cluster":"api"}],
		"clusters":[
			{"name":"web","endpoints":[{"address":%q},{"address":%q}],
			 "connect_timeout":"250ms","timeout":"2s",
			 "outlier_detection":{"consecutive_gateway_failures":1,"interval":"1s","base_ejection_time":"30s",
			   "max_ejection_time":"60s","max_ejection_percent":50,"success_rate_minimum_hosts":2,
			   "success_rate_request_volume":50,"success_rate_stdev_factor":1.9},
			 "retry":{"max_retries":1,"retry_on":[503],"per_try_timeout":"500ms","backoff_base":"1ms",
			   "backoff_max":"2ms","max_body_bytes":1024,"budget":{"ratio":0.2,"min_per_second":10,"window":"10s"}}},
			{"name":"api","endpoints":[{"address":%q}],"timeout":"2s",
			 "health_check":{"path":"/healthz","interval":"50ms","jitter":"1ms","timeout":"40ms",
			   "healthy_threshold":1,"unhealthy_threshold":1}}],
		"tls":{"certificates":[{"cert_file":"site.crt","key_file":"site.key"}],"handshake_timeout":"1s"}}`,
		goldenDeadAddr, webAddr, apiAddr)
	writeCfg(cfg)

	ctx, cancel := context.WithCancel(context.Background())
	hup := make(chan os.Signal, 1)
	addrs := make(chan [2]string, 1)
	done := make(chan error, 1)
	logs := &logBuf{}
	go func() {
		done <- run(ctx, env(map[string]string{"STEER_CONFIG": cfgPath, "STEER_ADMIN_TOKEN": token,
			"STEER_DRAIN_PERIOD": "0s", "STEER_SHUTDOWN_TIMEOUT": "2s"}), logs, hup,
			func(p, a string) { addrs <- [2]string{p, a} })
	}()
	var ad [2]string
	select {
	case ad = <-addrs:
	case err := <-done:
		t.Fatal(err)
	}
	scrape := func() string {
		t.Helper()
		code, body := get(t, "http://"+ad[1]+"/metrics", token)
		if code != http.StatusOK {
			t.Fatalf("/metrics: %d %s", code, body)
		}
		return body
	}
	waitFor := func(series string) {
		t.Helper()
		deadline := time.Now().Add(5 * time.Second)
		for !strings.Contains(scrape(), series) {
			if time.Now().After(deadline) {
				t.Fatalf("/metrics never showed %s:\n%s", series, scrape())
			}
			time.Sleep(10 * time.Millisecond)
		}
	}

	// api's endpoint fails its first probe, then recovers.
	waitFor(`steer_health_transitions_total{cluster="api",to="unhealthy"} 1`)
	healthy.Store(true)
	waitFor(`steer_health_transitions_total{cluster="api",to="healthy"} 1`)

	tr := &http.Transport{TLSClientConfig: &tls.Config{RootCAs: pool, ServerName: "steer.test"}}
	defer tr.CloseIdleConnections()
	client := &http.Client{Transport: tr, Timeout: 5 * time.Second}
	request := func(host string) int {
		t.Helper()
		req, _ := http.NewRequest(http.MethodGet, "https://"+ad[0]+"/", nil)
		req.Host = host
		resp, err := client.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
		return resp.StatusCode
	}
	// The dead endpoint is ejected after its first connect error, and the
	// retry goes to the live one.
	for range 4 {
		if code := request("web.test"); code != http.StatusOK {
			t.Fatalf("web.test: %d", code)
		}
	}
	waitFor(`steer_outlier_ejections_total{cluster="web",reason="consecutive_gateway_failure"} 1`)
	if code := request("api.test"); code != http.StatusServiceUnavailable {
		t.Fatalf("api.test: %d", code)
	}
	if code := request("nowhere.test"); code != http.StatusNotFound {
		t.Fatalf("nowhere.test: %d", code)
	}
	// Plain HTTP on the TLS listener fails its handshake.
	if resp, err := http.Get("http://" + ad[0] + "/"); err == nil {
		resp.Body.Close()
	}
	waitFor(`steer_tls_handshakes_total{result="error"} 1`)

	// One reload that applies and one that is refused.
	hup <- os.Interrupt
	waitLog(t, logs, `"msg":"reloaded on SIGHUP"`)
	writeCfg(`{"listen":`)
	hup <- os.Interrupt
	waitLog(t, logs, "configuration rejected")
	waitFor(`steer_config_reloads_total{result="failure"} 1`)

	out := scrape()
	cancel()
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	out = strings.NewReplacer(webAddr, goldenWebAddr, apiAddr, goldenAPIAddr).Replace(out)
	for _, want := range []string{
		`steer_config_reloads_total{result="success"} 1`,
		`steer_no_route_total 1`,
		`steer_requests_total{cluster="web",code_class="2xx"} 4`,
		`steer_requests_total{cluster="api",code_class="5xx"} 1`,
		`steer_retries_total{cluster="web"} 1`,
		`steer_upstream_attempts_total{cluster="web",endpoint="127.0.0.1:1",result="connect_error"} 1`,
		`steer_upstream_attempts_total{cluster="web",endpoint="127.0.0.1:19101",result="success"} 4`,
		`steer_upstream_attempts_total{cluster="api",endpoint="127.0.0.1:19102",result="http_5xx"} 1`,
		`steer_tls_handshakes_total{result="ok"} 1`,
		`steer_request_duration_seconds_count{cluster="web"} 4`,
		"steer_config_version 2",
		"steer_tls_handshakes_in_progress 0",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("missing %q in:\n%s", want, out)
		}
	}
	if *updateMetrics {
		if err := os.WriteFile(metricsGoldenPath, []byte(out), 0o644); err != nil {
			t.Fatal(err)
		}
		return
	}
	golden, err := os.ReadFile(metricsGoldenPath)
	if err != nil {
		t.Fatalf("%v (run with -update to write it)", err)
	}
	if got, want := metricsShape(out), metricsShape(string(golden)); got != want {
		t.Errorf("/metrics no longer matches %s; rerun with -update if the change is meant.\ngot:\n%s\nwant:\n%s",
			metricsGoldenPath, got, want)
	}
}
