package config

import (
	"crypto/tls"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

const full = `{
  "listen": ":8080",
  "admin_listen": "127.0.0.1:9901",
  "trusted_proxies": ["10.0.0.0/8", "192.168.1.7"],
  "routes": [
    {"host": "API.example.com", "path_prefix": "/v1/", "cluster": "api"},
    {"cluster": "web"}
  ],
  "clusters": [
    {
      "name": "api",
      "policy": "maglev",
      "hash_header": "x-user-id",
      "endpoints": [
        {"address": "10.0.1.1:9000", "weight": 3},
        {"address": "api-backup.internal:9000", "priority": 1}
      ],
      "connect_timeout": "250ms",
      "timeout": "0s",
      "max_requests": 50,
      "panic_threshold_percent": 0,
      "health_check": {"path": "/healthz", "interval": "2s", "jitter": "200ms", "timeout": "500ms",
                       "healthy_threshold": 1, "unhealthy_threshold": 3, "host": "health.internal"},
      "outlier_detection": {"consecutive_gateway_failures": 0, "interval": "1s", "base_ejection_time": "5s",
                            "max_ejection_percent": 50, "success_rate_minimum_hosts": 2,
                            "success_rate_request_volume": 20, "success_rate_stdev_factor": 1.5},
      "retry": {"max_retries": 3, "retry_on": [503], "per_try_timeout": "300ms", "backoff_base": "10ms",
                "backoff_max": "100ms", "max_body_bytes": 1024,
                "budget": {"ratio": 0.1, "min_per_second": 2, "window": "5s"}}
    },
    {"name": "web", "endpoints": [{"address": "[::1]:8081"}]}
  ]
}`

func TestParseFull(t *testing.T) {
	c, err := Parse([]byte(full))
	if err != nil {
		t.Fatal(err)
	}
	if c.Listen != ":8080" || len(c.TrustedProxies) != 2 || c.TrustedProxies[1].String() != "192.168.1.7/32" {
		t.Fatalf("top level %+v", c)
	}
	if c.Routes[0].Host != "api.example.com" || c.Routes[1].Host != "*" || c.Routes[1].PathPrefix != "/" {
		t.Fatalf("routes %+v", c.Routes)
	}
	api := c.Clusters[0]
	if api.Policy != Maglev || api.HashHeader != "X-User-Id" || api.Timeout != 0 || api.MaxRequests != 50 ||
		api.PanicThresholdPercent != 0 || api.ConnectTimeout != 250*time.Millisecond {
		t.Fatalf("api %+v", api)
	}
	if api.Endpoints[0].Weight != 3 || api.Endpoints[1].Priority != 1 || api.Endpoints[1].Weight != 1 {
		t.Fatalf("endpoints %+v", api.Endpoints)
	}
	if h := api.HealthCheck; h.Interval != 2*time.Second || h.HealthyThreshold != 1 || h.Host != "health.internal" {
		t.Fatalf("health %+v", h)
	}
	o := api.OutlierDetection
	if o.ConsecutiveGatewayFailures != 0 || o.MaxEjectionTime != DefaultMaxEjectionTime || o.SuccessRateStdevFactor != 1.5 {
		t.Fatalf("outlier %+v", o)
	}
	r := api.Retry
	if r.MaxRetries != 3 || len(r.RetryOn) != 1 || r.MaxBodyBytes != 1024 || r.Budget.Window != 5*time.Second || r.Budget.Ratio != 0.1 {
		t.Fatalf("retry %+v", r)
	}
	web := c.Clusters[1]
	if web.Policy != RoundRobin || web.Timeout != DefaultTimeout || web.MaxRequests != DefaultMaxRequests ||
		web.HealthCheck != nil || web.OutlierDetection != nil || web.Retry != nil || web.PanicThresholdPercent != 50 {
		t.Fatalf("web defaults %+v", web)
	}
}

func TestDefaults(t *testing.T) {
	c, err := Parse([]byte(`{"listen":":80","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}],
		"health_check":{"path":"/"},"outlier_detection":{},"retry":{"budget":{}}}]}`))
	if err != nil {
		t.Fatal(err)
	}
	if c.AdminListen != DefaultAdminListen {
		t.Fatalf("admin %q", c.AdminListen)
	}
	a := c.Clusters[0]
	if h := a.HealthCheck; h.Interval != DefaultHealthInterval || h.Timeout != DefaultHealthTimeout || h.UnhealthyThreshold != 2 {
		t.Fatalf("health %+v", h)
	}
	if o := a.OutlierDetection; o.ConsecutiveGatewayFailures != 5 || o.MaxEjectionPercent != 10 || o.SuccessRateMinimumHosts != 5 {
		t.Fatalf("outlier %+v", o)
	}
	if r := a.Retry; r.MaxRetries != 2 || len(r.RetryOn) != 3 || r.BackoffMax != DefaultBackoffMax || r.Budget.MinPerSecond != 10 {
		t.Fatalf("retry %+v", r)
	}
	// A short interval lowers the default probe timeout to fit.
	c, err = Parse([]byte(`{"listen":":80","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}],
		"health_check":{"path":"/","interval":"100ms"}}]}`))
	if err != nil || c.Clusters[0].HealthCheck.Timeout != 100*time.Millisecond {
		t.Fatalf("%v %+v", err, c)
	}
}

func TestErrors(t *testing.T) {
	base := func(cluster string) string {
		return `{"listen":":80","routes":[{"cluster":"a"}],"clusters":[` + cluster + `]}`
	}
	ep := `"endpoints":[{"address":"h:1"}]`
	cases := map[string]string{
		`not json`:                              "invalid JSON",
		`{"listen":":80"} {}`:                   "trailing data",
		`{"listen":":80","x":1}`:                "unknown field",
		`{}`:                                    "listen: is required",
		`{"listen":"8080"}`:                     "not host:port",
		`{"listen":":99999"}`:                   "port",
		`{"listen":"example.com:80"}`:           "must be an IP address",
		`{"listen":":80","admin_listen":":80"}`: "must differ",
		`{"listen":":80","admin_listen":"x"}`:   "admin_listen",
		`{"listen":":80","trusted_proxies":["10.0.0.1/8"]}`:                                                                                   "host bits set",
		`{"listen":":80","trusted_proxies":["nope"]}`:                                                                                         "not an IP address",
		`{"listen":":80","routes":[]}`:                                                                                                        "at least one route",
		`{"listen":":80","clusters":[]}`:                                                                                                      "at least one cluster",
		base(`{"name":"a",` + ep + `,"timeout":5}`):                                                                                           "durations are strings",
		base(`{"name":"a",` + ep + `,"timeout":"soon"}`):                                                                                      "invalid duration",
		base(`{"name":"a b",` + ep + `}`):                                                                                                     "name: must be 1 to 64",
		base(`{"name":"a",` + ep + `},{"name":"a",` + ep + `}`):                                                                               "duplicate cluster",
		base(`{"name":"a","policy":"random",` + ep + `}`):                                                                                     "policy",
		base(`{"name":"a","policy":"maglev",` + ep + `}`):                                                                                     "hash_header: is required",
		base(`{"name":"a","policy":"maglev","hash_header":"bad header",` + ep + `}`):                                                          "hash_header",
		base(`{"name":"a","hash_header":"X-Id",` + ep + `}`):                                                                                  "only used with the maglev",
		base(`{"name":"a","endpoints":[]}`):                                                                                                   "at least one endpoint",
		base(`{"name":"a","endpoints":[{"address":"h"}]}`):                                                                                    "not host:port",
		base(`{"name":"a","endpoints":[{"address":"h:0"}]}`):                                                                                  "port",
		base(`{"name":"a","endpoints":[{"address":"h:01"}]}`):                                                                                 "port",
		base(`{"name":"a","endpoints":[{"address":"-h:1"}]}`):                                                                                 "not a valid DNS name",
		base(`{"name":"a","endpoints":[{"address":"h_x:1"}]}`):                                                                                "not a valid DNS name",
		base(`{"name":"a","endpoints":[{"address":":1"}]}`):                                                                                   "1 to 253",
		base(`{"name":"a","endpoints":[{"address":"h:1"},{"address":"h:1"}]}`):                                                                "duplicate address",
		base(`{"name":"a","endpoints":[{"address":"h:1","weight":0}]}`):                                                                       "weight: must be between 1 and 1000",
		base(`{"name":"a","endpoints":[{"address":"h:1","priority":8}]}`):                                                                     "priority",
		base(`{"name":"a",` + ep + `,"max_requests":0}`):                                                                                      "max_requests",
		base(`{"name":"a",` + ep + `,"connect_timeout":"0s"}`):                                                                                "connect_timeout",
		base(`{"name":"a",` + ep + `,"panic_threshold_percent":101}`):                                                                         "panic_threshold_percent",
		base(`{"name":"a",` + ep + `,"health_check":{"path":"healthz"}}`):                                                                     "path",
		base(`{"name":"a",` + ep + `,"health_check":{"path":"/a b"}}`):                                                                        "path",
		base(`{"name":"a",` + ep + `,"health_check":{"path":"/","host":"a b"}}`):                                                              "host",
		base(`{"name":"a",` + ep + `,"health_check":{"path":"/","interval":"1s","timeout":"2s"}}`):                                            "must not exceed interval",
		base(`{"name":"a",` + ep + `,"health_check":{"path":"/","interval":"1s","jitter":"2s"}}`):                                             "must not exceed interval",
		base(`{"name":"a",` + ep + `,"health_check":{"path":"/","healthy_threshold":0}}`):                                                     "healthy_threshold",
		base(`{"name":"a",` + ep + `,"outlier_detection":{"base_ejection_time":"10s","max_ejection_time":"5s"}}`):                             "not be less than base",
		base(`{"name":"a",` + ep + `,"outlier_detection":{"success_rate_stdev_factor":0}}`):                                                   "stdev_factor",
		base(`{"name":"a",` + ep + `,"outlier_detection":{"max_ejection_percent":-1}}`):                                                       "max_ejection_percent",
		base(`{"name":"a",` + ep + `,"retry":{"max_retries":6}}`):                                                                             "max_retries",
		base(`{"name":"a",` + ep + `,"retry":{"retry_on":[404]}}`):                                                                            "5xx",
		base(`{"name":"a",` + ep + `,"retry":{"retry_on":[503,503]}}`):                                                                        "duplicate status",
		base(`{"name":"a",` + ep + `,"timeout":"1s","retry":{"per_try_timeout":"2s"}}`):                                                       "must not exceed the cluster timeout",
		base(`{"name":"a",` + ep + `,"retry":{"backoff_base":"1s","backoff_max":"10ms"}}`):                                                    "backoff_max",
		base(`{"name":"a",` + ep + `,"retry":{"max_body_bytes":-1}}`):                                                                         "max_body_bytes",
		base(`{"name":"a",` + ep + `,"retry":{"budget":{"ratio":1.5}}}`):                                                                      "ratio",
		base(`{"name":"a",` + ep + `,"retry":{"budget":{"window":"1500ms"}}}`):                                                                "whole number of seconds",
		base(`{"name":"a",` + ep + `,"retry":{"budget":{"min_per_second":-1}}}`):                                                              "min_per_second",
		`{"listen":":80","routes":[{"cluster":"b"}],"clusters":[{"name":"a",` + ep + `}]}`:                                                    "unknown cluster",
		`{"listen":":80","routes":[{"cluster":"a"},{"cluster":"a"}],"clusters":[{"name":"a",` + ep + `}]}`:                                    "duplicate route",
		`{"listen":":80","routes":[{"cluster":"a","host":"a.com:80"}],"clusters":[{"name":"a",` + ep + `}]}`:                                  "no port",
		`{"listen":":80","routes":[{"cluster":"a","host":"*.*.com"}],"clusters":[{"name":"a",` + ep + `}]}`:                                   "*.suffix",
		`{"listen":":80","routes":[{"cluster":"a","path_prefix":"api"}],"clusters":[{"name":"a",` + ep + `}]}`:                                "must start with",
		`{"listen":":80","routes":[{"cluster":"a","path_prefix":"/a/../b"}],"clusters":[{"name":"a",` + ep + `}]}`:                            "segments",
		`{"listen":":80","routes":[{"cluster":"a","path_prefix":"//a"}],"clusters":[{"name":"a",` + ep + `}]}`:                                "segments",
		`{"listen":":80","routes":[{"cluster":"a","path_prefix":"/a%2f"}],"clusters":[{"name":"a",` + ep + `}]}`:                              "must not contain",
		`{"listen":":80","routes":[{"cluster":"a","path_prefix":"/` + strings.Repeat("a", 1100) + `"}],"clusters":[{"name":"a",` + ep + `}]}`: "at most 1024",
	}
	for in, want := range cases {
		_, err := Parse([]byte(in))
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("Parse(%s)\n  err = %v\n  want it to mention %q", in, err, want)
		}
	}
}

func TestManyErrorsAreCapped(t *testing.T) {
	var b strings.Builder
	b.WriteString(`{"listen":":80","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[`)
	for i := 0; i < 40; i++ {
		if i > 0 {
			b.WriteString(",")
		}
		b.WriteString(`{"address":"bad"}`)
	}
	b.WriteString(`]}]}`)
	_, err := Parse([]byte(b.String()))
	if err == nil || !strings.Contains(err.Error(), "more errors)") || strings.Count(err.Error(), "\n") != maxErrors+1 {
		t.Fatalf("err %v", err)
	}
}

func TestSizeLimits(t *testing.T) {
	if _, err := Parse(make([]byte, MaxFileBytes+1)); err == nil || !strings.Contains(err.Error(), "larger") {
		t.Fatalf("oversized: %v", err)
	}
	dir := t.TempDir()
	big := filepath.Join(dir, "big.json")
	os.WriteFile(big, make([]byte, MaxFileBytes+10), 0o600)
	if _, err := Load(big); err == nil || !strings.Contains(err.Error(), "larger") {
		t.Fatalf("oversized file: %v", err)
	}
	if _, err := Load(filepath.Join(dir, "missing.json")); err == nil {
		t.Fatal("missing file loaded")
	}
	ok := filepath.Join(dir, "ok.json")
	os.WriteFile(ok, []byte(full), 0o600)
	if _, err := Load(ok); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(dir); err == nil {
		t.Fatal("directory loaded")
	}
	// Count limits.
	var b strings.Builder
	b.WriteString(`{"listen":":80","routes":[{"cluster":"a"}],"clusters":[`)
	for i := 0; i <= MaxClusters; i++ {
		if i > 0 {
			b.WriteString(",")
		}
		b.WriteString(`{"name":"c` + strings.Repeat("x", i%3) + `","endpoints":[{"address":"h:1"}]}`)
	}
	b.WriteString(`]}`)
	if _, err := Parse([]byte(b.String())); err == nil || !strings.Contains(err.Error(), "at most 256 clusters") {
		t.Fatalf("cluster limit: %v", err)
	}
}

func TestHelpers(t *testing.T) {
	if validToken("") || validToken(strings.Repeat("a", 65)) || !validToken("X-Request_Id.2~") {
		t.Fatal("validToken")
	}
	if checkHostname(strings.Repeat("a", 64)) == nil || checkHostname("a..b") == nil || checkHostname("ok-1.b2") != nil {
		t.Fatal("checkHostname")
	}
	if checkListen("localhost:0") != nil || checkListen("[::1]:80") != nil {
		t.Fatal("checkListen")
	}
	if _, err := parsePrefix("fe80::1%eth0"); err == nil {
		t.Fatal("zoned address accepted")
	}
}

// Any input either fails with an error or yields a config that satisfies the
// invariants the proxy relies on.
func FuzzParse(f *testing.F) {
	f.Add([]byte(full))
	f.Add([]byte(`{"listen":":80","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}]}`))
	f.Add([]byte(`{"listen":":443","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}],
		"tls":{"certificates":[{"cert_file":"a","key_file":"b","server_names":["*.a.com"]}],"cipher_suites":["TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"],"min_version":"1.2"}}`))
	f.Add([]byte(`{"listen":":80","clusters":[{"name":"a","policy":"maglev","hash_header":"k","endpoints":[{"address":"1.2.3.4:5","weight":1000,"priority":7}]}]}`))
	f.Fuzz(func(t *testing.T, data []byte) {
		c, err := Parse(data)
		if err != nil {
			return
		}
		names := map[string]bool{}
		for _, cl := range c.Clusters {
			names[cl.Name] = true
			if len(cl.Endpoints) == 0 || len(cl.Endpoints) > MaxEndpoints || cl.MaxRequests < 1 {
				t.Fatalf("cluster %+v", cl)
			}
			for _, e := range cl.Endpoints {
				if e.Weight < 1 || e.Weight > MaxWeight || e.Priority < 0 || e.Priority > MaxPriority {
					t.Fatalf("endpoint %+v", e)
				}
			}
			if (cl.Policy == Maglev) != (cl.HashHeader != "") {
				t.Fatalf("hash header %+v", cl)
			}
			if r := cl.Retry; r != nil && (r.BackoffBase <= 0 || r.BackoffMax < r.BackoffBase || r.Budget.Window < time.Second) {
				t.Fatalf("retry %+v", r)
			}
			if h := cl.HealthCheck; h != nil && (h.Interval <= 0 || h.Timeout > h.Interval || h.HealthyThreshold < 1) {
				t.Fatalf("health %+v", h)
			}
			if o := cl.OutlierDetection; o != nil && (o.BaseEjectionTime <= 0 || o.MaxEjectionTime < o.BaseEjectionTime) {
				t.Fatalf("outlier %+v", o)
			}
		}
		if tc := c.TLS; tc != nil {
			if len(tc.Certificates) == 0 || tc.HandshakeTimeout < MinHandshakeTimeout || tc.MaxConcurrentHandshakes < 1 ||
				(tc.MinVersion == tls.VersionTLS13) != (tc.CipherSuites == nil) {
				t.Fatalf("tls %+v", tc)
			}
			for _, cc := range tc.Certificates {
				if cc.CertFile == "" || cc.KeyFile == "" || (cc.ServerNames != nil && len(cc.ServerNames) == 0) {
					t.Fatalf("certificate %+v", cc)
				}
				for _, n := range cc.ServerNames {
					if n != strings.ToLower(n) || n == "*" || checkHostPattern(n) != nil {
						t.Fatalf("server name %q", n)
					}
				}
			}
		}
		for _, r := range c.Routes {
			if !names[r.Cluster] || !strings.HasPrefix(r.PathPrefix, "/") || r.Host == "" {
				t.Fatalf("route %+v", r)
			}
		}
	})
}

func TestTLS(t *testing.T) {
	const pre = `{"listen":":443","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}],"tls":`
	c, err := Parse([]byte(pre + `{"certificates":[{"cert_file":"a.crt","key_file":"a.key"}]}}`))
	if err != nil {
		t.Fatal(err)
	}
	tc := c.TLS
	if tc.MinVersion != tls.VersionTLS12 || tc.HandshakeTimeout != DefaultHandshakeTimeout ||
		tc.MaxConcurrentHandshakes != DefaultMaxConcurrentHandshakes || tc.StrictSNI ||
		len(tc.CipherSuites) != len(cipherPolicies[CipherModern]) || tc.Certificates[0].ServerNames != nil {
		t.Fatalf("defaults %+v", tc)
	}
	for _, id := range tc.CipherSuites {
		if strings.Contains(tls.CipherSuiteName(id), "CBC") {
			t.Fatalf("modern policy has %s", tls.CipherSuiteName(id))
		}
	}

	c, err = Parse([]byte(pre + `{"certificates":[{"cert_file":"/a.crt","key_file":"/a.key","server_names":["API.example.com","*.example.org"]}],
		"min_version":"1.2","cipher_policy":"compatible","handshake_timeout":"2s","max_concurrent_handshakes":10,"strict_sni":true}}`))
	if err != nil {
		t.Fatal(err)
	}
	tc = c.TLS
	if len(tc.CipherSuites) != len(cipherPolicies[CipherCompatible]) || tc.HandshakeTimeout != 2*time.Second ||
		tc.MaxConcurrentHandshakes != 10 || !tc.StrictSNI ||
		strings.Join(tc.Certificates[0].ServerNames, ",") != "api.example.com,*.example.org" {
		t.Fatalf("explicit %+v", tc)
	}

	c, err = Parse([]byte(pre + `{"certificates":[{"cert_file":"a","key_file":"b"}],"cipher_suites":["TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256","TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"]}}`))
	if err != nil {
		t.Fatal(err)
	}
	if got := c.TLS.CipherSuites; len(got) != 2 || got[0] != tls.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 {
		t.Fatalf("suites %v", got)
	}

	c, err = Parse([]byte(pre + `{"certificates":[{"cert_file":"a","key_file":"b"}],"min_version":"1.3"}}`))
	if err != nil || c.TLS.MinVersion != tls.VersionTLS13 || c.TLS.CipherSuites != nil {
		t.Fatalf("1.3: %v %+v", err, c)
	}

	cert := `"certificates":[{"cert_file":"a","key_file":"b"}]`
	cases := map[string]string{
		`{}`:                                                                           "tls.certificates: at least one certificate",
		`{"certificates":[{}]}`:                                                        "cert_file: is required",
		`{"certificates":[{"cert_file":"a"}]}`:                                         "key_file: is required",
		`{` + cert + `,"min_version":"1.1"}`:                                           "min_version",
		`{` + cert + `,"cipher_policy":"weak"}`:                                        "cipher_policy",
		`{` + cert + `,"cipher_suites":[]}`:                                            "cipher_suites: must not be empty",
		`{` + cert + `,"cipher_suites":["x"]}`:                                         "not a supported secure cipher suite",
		`{` + cert + `,"cipher_suites":["TLS_RSA_WITH_RC4_128_SHA"]}`:                  "not a supported secure",
		`{` + cert + `,"cipher_suites":["TLS_RSA_WITH_AES_128_GCM_SHA256"]}`:           "not a supported secure",
		`{` + cert + `,"cipher_suites":["TLS_AES_128_GCM_SHA256"]}`:                    "TLS 1.3 suite",
		`{` + cert + `,"cipher_policy":"modern","cipher_suites":["x"]}`:                "cannot be combined",
		`{` + cert + `,"min_version":"1.3","cipher_policy":"modern"}`:                  "not configurable",
		`{` + cert + `,"handshake_timeout":"10ms"}`:                                    "handshake_timeout",
		`{` + cert + `,"handshake_timeout":"2m"}`:                                      "handshake_timeout",
		`{` + cert + `,"max_concurrent_handshakes":0}`:                                 "max_concurrent_handshakes",
		`{"certificates":[{"cert_file":"a","key_file":"b","server_names":[]}]}`:        "must not be empty",
		`{"certificates":[{"cert_file":"a","key_file":"b","server_names":["*"]}]}`:     "host name or *.suffix",
		`{"certificates":[{"cert_file":"a","key_file":"b","server_names":["a:1"]}]}`:   "host name or *.suffix",
		`{"certificates":[{"cert_file":"a","key_file":"b","server_names":["*.*.a"]}]}`: "host name or *.suffix",
		`{"certificates":[{"cert_file":"a","key_file":"b"}],"cipher_suites":["TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256","TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"]}`: "duplicate cipher suite",
		`{"certificates":[{"cert_file":"a","key_file":"b"}],"ocsp":true}`:                                                                                       "unknown field",
	}
	for in, want := range cases {
		_, err := Parse([]byte(pre + in + `}`))
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("tls %s\n  err = %v\n  want it to mention %q", in, err, want)
		}
	}
	var many strings.Builder
	many.WriteString(`{"certificates":[`)
	for i := 0; i <= MaxCertificates; i++ {
		if i > 0 {
			many.WriteString(",")
		}
		many.WriteString(`{"cert_file":"a","key_file":"b"}`)
	}
	many.WriteString(`]}`)
	if _, err := Parse([]byte(pre + many.String() + `}`)); err == nil || !strings.Contains(err.Error(), "at most 256 certificates") {
		t.Errorf("too many certificates: %v", err)
	}
}

func TestLoadResolvesCertificatePaths(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "steer.json")
	js := `{"listen":":443","routes":[{"cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}],
		"tls":{"certificates":[{"cert_file":"certs/a.crt","key_file":"/abs/a.key"}]}}`
	if err := os.WriteFile(path, []byte(js), 0o600); err != nil {
		t.Fatal(err)
	}
	c, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := c.TLS.Certificates[0]; got.CertFile != filepath.Join(dir, "certs", "a.crt") || got.KeyFile != "/abs/a.key" {
		t.Fatalf("paths %+v", got)
	}
}

// The shipped example configurations must always load.
func TestExamplesParse(t *testing.T) {
	files, err := filepath.Glob("../../examples/*.json")
	if err != nil || len(files) < 3 {
		t.Fatalf("examples: %v %v", files, err)
	}
	for _, f := range files {
		if _, err := Load(f); err != nil {
			t.Errorf("%s: %v", f, err)
		}
	}
}
