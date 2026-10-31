// Package config parses and validates steer's JSON configuration file.
//
// Parsing is strict: unknown fields, trailing data, out-of-range values and
// dangling references are all rejected, with every problem reported by its
// JSON path. A Config returned without error has every default applied and is
// safe to build a proxy from.
package config

import (
	"bytes"
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/netip"
	"net/textproto"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

// Limits on the size of a configuration.
const (
	MaxFileBytes      = 1 << 20
	MaxRoutes         = 1024
	MaxClusters       = 256
	MaxEndpoints      = 1024 // per cluster
	MaxPriority       = 7
	MaxWeight         = 1000
	MaxTrustedProxies = 64
	MaxRetries        = 5
	MaxRetryBodyBytes = 16 << 20
	MaxPathPrefixLen  = 1024
	maxErrors         = 20

	MaxCertificates           = 256
	MaxServerNamesPerCert     = 100
	MaxHandshakeTimeout       = time.Minute
	MinHandshakeTimeout       = 100 * time.Millisecond
	MaxConcurrentHandshakeCap = 100_000
)

// Defaults applied when a field is absent.
const (
	DefaultAdminListen           = "127.0.0.1:9901"
	DefaultConnectTimeout        = time.Second
	DefaultTimeout               = 15 * time.Second
	DefaultMaxRequests           = 1024
	DefaultPanicThresholdPercent = 50

	DefaultHealthInterval = 5 * time.Second
	DefaultHealthTimeout  = time.Second
	DefaultHealthyAfter   = 2
	DefaultUnhealthyAfter = 2

	DefaultConsecutiveGatewayFailures = 5
	DefaultOutlierInterval            = 10 * time.Second
	DefaultBaseEjectionTime           = 30 * time.Second
	DefaultMaxEjectionTime            = 300 * time.Second
	DefaultMaxEjectionPercent         = 10
	DefaultSuccessRateMinimumHosts    = 5
	DefaultSuccessRateRequestVolume   = 100
	DefaultSuccessRateStdevFactor     = 1.9

	DefaultMaxRetries      = 2
	DefaultBackoffBase     = 25 * time.Millisecond
	DefaultBackoffMax      = 250 * time.Millisecond
	DefaultRetryBodyBytes  = 64 << 10
	DefaultBudgetRatio     = 0.2
	DefaultBudgetMinPerSec = 10
	DefaultBudgetWindow    = 10 * time.Second

	DefaultHandshakeTimeout        = 5 * time.Second
	DefaultMaxConcurrentHandshakes = 1024
)

// DefaultRetryOn is the set of upstream statuses retried when retry_on is absent.
var DefaultRetryOn = []int{502, 503, 504}

// Policy names a load-balancing policy.
type Policy string

// Load-balancing policies.
const (
	RoundRobin  Policy = "round_robin"  // smooth weighted round robin
	LeastLoaded Policy = "least_loaded" // power of two choices on in-flight x EWMA latency
	Maglev      Policy = "maglev"       // consistent hashing on a request header
)

// Config is a validated configuration with defaults applied.
type Config struct {
	Listen         string
	AdminListen    string
	TrustedProxies []netip.Prefix
	TLS            *TLS // nil serves plain HTTP
	Routes         []Route
	Clusters       []Cluster
}

// Cipher policies for TLS 1.2. TLS 1.3 suites are fixed by the protocol
// implementation and are always all enabled.
const (
	CipherModern     = "modern"     // ECDHE with AES-GCM or ChaCha20-Poly1305 only
	CipherCompatible = "compatible" // modern plus ECDHE with AES-CBC, for older clients
)

// TLS configures TLS termination on the proxy listener.
type TLS struct {
	Certificates []Certificate
	MinVersion   uint16 // tls.VersionTLS12 or tls.VersionTLS13
	// CipherSuites are the TLS 1.2 suites in preference order; nil when
	// MinVersion is TLS 1.3.
	CipherSuites            []uint16
	HandshakeTimeout        time.Duration
	MaxConcurrentHandshakes int
	// StrictSNI refuses handshakes whose server name matches no certificate
	// (and handshakes without one) instead of answering with the first
	// certificate.
	StrictSNI bool
}

// Certificate is a PEM certificate chain and its private key. ServerNames
// overrides the names taken from the certificate's DNS SANs.
type Certificate struct {
	CertFile    string
	KeyFile     string
	ServerNames []string // lower-case names or *.suffix wildcards
}

// cipherPolicies lists the TLS 1.2 suites of each policy, in preference order.
var cipherPolicies = map[string][]uint16{
	CipherModern: {
		tls.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, tls.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
		tls.TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384, tls.TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
		tls.TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, tls.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
	},
	CipherCompatible: {
		tls.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, tls.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
		tls.TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384, tls.TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
		tls.TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, tls.TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
		tls.TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA, tls.TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA,
		tls.TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA, tls.TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA,
	},
}

// Route sends requests for Host whose path starts with PathPrefix to Cluster.
// Host is "*", an exact lower-case name, or "*.suffix".
type Route struct {
	Host       string
	PathPrefix string
	Cluster    string
}

// Cluster is a named group of endpoints and the policies used to reach them.
type Cluster struct {
	Name                  string
	Policy                Policy
	HashHeader            string // canonical header name; set only for Maglev
	Endpoints             []Endpoint
	ConnectTimeout        time.Duration
	Timeout               time.Duration // whole request; 0 means none
	MaxRequests           int
	PanicThresholdPercent int // 0 disables panic routing
	HealthCheck           *HealthCheck
	OutlierDetection      *OutlierDetection
	Retry                 *Retry
}

// Endpoint is one upstream address.
type Endpoint struct {
	Address  string
	Weight   int
	Priority int
}

// HealthCheck configures active HTTP health checking.
type HealthCheck struct {
	Path               string
	Host               string // Host header for probes; empty uses the endpoint address
	Interval           time.Duration
	Jitter             time.Duration
	Timeout            time.Duration
	HealthyThreshold   int
	UnhealthyThreshold int
}

// OutlierDetection configures passive, Envoy-style outlier ejection.
type OutlierDetection struct {
	ConsecutiveGatewayFailures int // 0 disables
	Interval                   time.Duration
	BaseEjectionTime           time.Duration
	MaxEjectionTime            time.Duration
	MaxEjectionPercent         int
	SuccessRateMinimumHosts    int // 0 disables success-rate ejection
	SuccessRateRequestVolume   int
	SuccessRateStdevFactor     float64
}

// Retry configures retries for a cluster.
type Retry struct {
	MaxRetries    int
	RetryOn       []int
	PerTryTimeout time.Duration // time to response headers; 0 means none
	BackoffBase   time.Duration
	BackoffMax    time.Duration
	MaxBodyBytes  int64
	Budget        Budget
}

// Budget limits retries to Ratio of requests plus MinPerSecond, over Window.
type Budget struct {
	Ratio        float64
	MinPerSecond int
	Window       time.Duration
}

// Duration is a JSON string in time.ParseDuration syntax, such as "250ms".
type Duration struct{ time.Duration }

// UnmarshalJSON accepts only strings.
func (d *Duration) UnmarshalJSON(b []byte) error {
	var s string
	if err := json.Unmarshal(b, &s); err != nil {
		return errors.New(`durations are strings such as "250ms" or "5s"`)
	}
	v, err := time.ParseDuration(s)
	if err != nil {
		return fmt.Errorf("invalid duration %q", s)
	}
	d.Duration = v
	return nil
}

type rawConfig struct {
	Listen         string       `json:"listen"`
	AdminListen    string       `json:"admin_listen"`
	TrustedProxies []string     `json:"trusted_proxies"`
	TLS            *rawTLS      `json:"tls"`
	Routes         []rawRoute   `json:"routes"`
	Clusters       []rawCluster `json:"clusters"`
}

type rawTLS struct {
	Certificates            []rawCertificate `json:"certificates"`
	MinVersion              string           `json:"min_version"`
	CipherPolicy            string           `json:"cipher_policy"`
	CipherSuites            []string         `json:"cipher_suites"`
	HandshakeTimeout        *Duration        `json:"handshake_timeout"`
	MaxConcurrentHandshakes *int             `json:"max_concurrent_handshakes"`
	StrictSNI               bool             `json:"strict_sni"`
}

type rawCertificate struct {
	CertFile    string   `json:"cert_file"`
	KeyFile     string   `json:"key_file"`
	ServerNames []string `json:"server_names"`
}

type rawRoute struct {
	Host       string `json:"host"`
	PathPrefix string `json:"path_prefix"`
	Cluster    string `json:"cluster"`
}

type rawCluster struct {
	Name                  string        `json:"name"`
	Policy                string        `json:"policy"`
	HashHeader            string        `json:"hash_header"`
	Endpoints             []rawEndpoint `json:"endpoints"`
	ConnectTimeout        *Duration     `json:"connect_timeout"`
	Timeout               *Duration     `json:"timeout"`
	MaxRequests           *int          `json:"max_requests"`
	PanicThresholdPercent *int          `json:"panic_threshold_percent"`
	HealthCheck           *rawHealth    `json:"health_check"`
	OutlierDetection      *rawOutlier   `json:"outlier_detection"`
	Retry                 *rawRetry     `json:"retry"`
}

type rawEndpoint struct {
	Address  string `json:"address"`
	Weight   *int   `json:"weight"`
	Priority *int   `json:"priority"`
}

type rawHealth struct {
	Path               string    `json:"path"`
	Host               string    `json:"host"`
	Interval           *Duration `json:"interval"`
	Jitter             *Duration `json:"jitter"`
	Timeout            *Duration `json:"timeout"`
	HealthyThreshold   *int      `json:"healthy_threshold"`
	UnhealthyThreshold *int      `json:"unhealthy_threshold"`
}

type rawOutlier struct {
	ConsecutiveGatewayFailures *int      `json:"consecutive_gateway_failures"`
	Interval                   *Duration `json:"interval"`
	BaseEjectionTime           *Duration `json:"base_ejection_time"`
	MaxEjectionTime            *Duration `json:"max_ejection_time"`
	MaxEjectionPercent         *int      `json:"max_ejection_percent"`
	SuccessRateMinimumHosts    *int      `json:"success_rate_minimum_hosts"`
	SuccessRateRequestVolume   *int      `json:"success_rate_request_volume"`
	SuccessRateStdevFactor     *float64  `json:"success_rate_stdev_factor"`
}

type rawRetry struct {
	MaxRetries    *int       `json:"max_retries"`
	RetryOn       []int      `json:"retry_on"`
	PerTryTimeout *Duration  `json:"per_try_timeout"`
	BackoffBase   *Duration  `json:"backoff_base"`
	BackoffMax    *Duration  `json:"backoff_max"`
	MaxBodyBytes  *int64     `json:"max_body_bytes"`
	Budget        *rawBudget `json:"budget"`
}

type rawBudget struct {
	Ratio        *float64  `json:"ratio"`
	MinPerSecond *int      `json:"min_per_second"`
	Window       *Duration `json:"window"`
}

// Load reads and parses the file at path. Relative certificate and key paths
// are resolved against the directory that holds the file.
func Load(path string) (*Config, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, MaxFileBytes+1))
	if err != nil {
		return nil, fmt.Errorf("reading %s: %w", path, err)
	}
	if len(data) > MaxFileBytes {
		return nil, fmt.Errorf("%s is larger than %d bytes", path, MaxFileBytes)
	}
	cfg, err := Parse(data)
	if err != nil {
		return nil, err
	}
	if cfg.TLS != nil {
		dir := filepath.Dir(path)
		for i := range cfg.TLS.Certificates {
			c := &cfg.TLS.Certificates[i]
			for _, f := range []*string{&c.CertFile, &c.KeyFile} {
				if !filepath.IsAbs(*f) {
					*f = filepath.Join(dir, *f)
				}
			}
		}
	}
	return cfg, nil
}

// Parse decodes and validates a configuration.
func Parse(data []byte) (*Config, error) {
	if len(data) > MaxFileBytes {
		return nil, fmt.Errorf("configuration is larger than %d bytes", MaxFileBytes)
	}
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	var raw rawConfig
	if err := dec.Decode(&raw); err != nil {
		return nil, fmt.Errorf("invalid JSON: %w", err)
	}
	if _, err := dec.Token(); err != io.EOF {
		return nil, errors.New("invalid JSON: trailing data after the configuration object")
	}
	v := &validator{}
	cfg := v.config(&raw)
	if err := v.err(); err != nil {
		return nil, err
	}
	return cfg, nil
}

type validator struct {
	errs    []string
	dropped int
}

func (v *validator) add(path, format string, args ...any) {
	if len(v.errs) >= maxErrors {
		v.dropped++
		return
	}
	v.errs = append(v.errs, path+": "+fmt.Sprintf(format, args...))
}

func (v *validator) err() error {
	if len(v.errs) == 0 {
		return nil
	}
	msg := strings.Join(v.errs, "\n")
	if v.dropped > 0 {
		msg += fmt.Sprintf("\n(%d more errors)", v.dropped)
	}
	return errors.New("invalid configuration:\n" + msg)
}

func (v *validator) intIn(path string, p *int, def, lo, hi int) int {
	if p == nil {
		return def
	}
	if *p < lo || *p > hi {
		v.add(path, "must be between %d and %d", lo, hi)
	}
	return *p
}

func (v *validator) durIn(path string, p *Duration, def, lo, hi time.Duration) time.Duration {
	if p == nil {
		return def
	}
	if p.Duration < lo || p.Duration > hi {
		v.add(path, "must be between %s and %s", lo, hi)
	}
	return p.Duration
}

func (v *validator) config(raw *rawConfig) *Config {
	cfg := &Config{Listen: raw.Listen, AdminListen: raw.AdminListen}
	if cfg.Listen == "" {
		v.add("listen", "is required, e.g. \":8080\"")
	} else if err := checkListen(cfg.Listen); err != nil {
		v.add("listen", "%v", err)
	}
	if cfg.AdminListen == "" {
		cfg.AdminListen = DefaultAdminListen
	} else if err := checkListen(cfg.AdminListen); err != nil {
		v.add("admin_listen", "%v", err)
	}
	if cfg.Listen != "" && cfg.Listen == cfg.AdminListen && !strings.HasSuffix(cfg.Listen, ":0") {
		v.add("admin_listen", "must differ from listen")
	}

	if len(raw.TrustedProxies) > MaxTrustedProxies {
		v.add("trusted_proxies", "at most %d entries", MaxTrustedProxies)
	}
	for i, s := range raw.TrustedProxies {
		p, err := parsePrefix(s)
		if err != nil {
			v.add(fmt.Sprintf("trusted_proxies[%d]", i), "%v", err)
			continue
		}
		cfg.TrustedProxies = append(cfg.TrustedProxies, p)
	}
	if raw.TLS != nil {
		cfg.TLS = v.tls("tls", raw.TLS)
	}

	names := map[string]bool{}
	if len(raw.Clusters) == 0 {
		v.add("clusters", "at least one cluster is required")
	}
	if len(raw.Clusters) > MaxClusters {
		v.add("clusters", "at most %d clusters", MaxClusters)
	}
	for i := range raw.Clusters {
		path := fmt.Sprintf("clusters[%d]", i)
		c := v.cluster(path, &raw.Clusters[i])
		if names[c.Name] {
			v.add(path+".name", "duplicate cluster %q", c.Name)
		}
		names[c.Name] = true
		cfg.Clusters = append(cfg.Clusters, c)
	}

	if len(raw.Routes) == 0 {
		v.add("routes", "at least one route is required")
	}
	if len(raw.Routes) > MaxRoutes {
		v.add("routes", "at most %d routes", MaxRoutes)
	}
	seen := map[[2]string]bool{}
	for i, rr := range raw.Routes {
		path := fmt.Sprintf("routes[%d]", i)
		r := Route{Host: strings.ToLower(rr.Host), PathPrefix: rr.PathPrefix, Cluster: rr.Cluster}
		if r.Host == "" {
			r.Host = "*"
		}
		if err := checkHostPattern(r.Host); err != nil {
			v.add(path+".host", "%v", err)
		}
		if r.PathPrefix == "" {
			r.PathPrefix = "/"
		}
		if err := checkPathPrefix(r.PathPrefix); err != nil {
			v.add(path+".path_prefix", "%v", err)
		}
		if !names[r.Cluster] {
			v.add(path+".cluster", "unknown cluster %q", r.Cluster)
		}
		k := [2]string{r.Host, r.PathPrefix}
		if seen[k] {
			v.add(path, "duplicate route for host %q and path_prefix %q", r.Host, r.PathPrefix)
		}
		seen[k] = true
		cfg.Routes = append(cfg.Routes, r)
	}
	return cfg
}

func (v *validator) tls(path string, rt *rawTLS) *TLS {
	t := &TLS{StrictSNI: rt.StrictSNI}
	switch rt.MinVersion {
	case "", "1.2":
		t.MinVersion = tls.VersionTLS12
	case "1.3":
		t.MinVersion = tls.VersionTLS13
	default:
		v.add(path+".min_version", `must be "1.2" or "1.3"`)
	}
	switch {
	case rt.CipherPolicy != "" && rt.CipherSuites != nil:
		v.add(path+".cipher_suites", "cannot be combined with cipher_policy")
	case rt.CipherSuites != nil:
		t.CipherSuites = v.cipherSuites(path+".cipher_suites", rt.CipherSuites)
	default:
		policy := rt.CipherPolicy
		if policy == "" {
			policy = CipherModern
		}
		suites, ok := cipherPolicies[policy]
		if !ok {
			v.add(path+".cipher_policy", "must be %q or %q", CipherModern, CipherCompatible)
		}
		t.CipherSuites = append([]uint16(nil), suites...)
	}
	if t.MinVersion == tls.VersionTLS13 {
		if rt.CipherPolicy != "" || rt.CipherSuites != nil {
			v.add(path+".min_version", "TLS 1.3 suites are not configurable; drop cipher_policy and cipher_suites")
		}
		t.CipherSuites = nil
	}
	t.HandshakeTimeout = v.durIn(path+".handshake_timeout", rt.HandshakeTimeout, DefaultHandshakeTimeout, MinHandshakeTimeout, MaxHandshakeTimeout)
	t.MaxConcurrentHandshakes = v.intIn(path+".max_concurrent_handshakes", rt.MaxConcurrentHandshakes, DefaultMaxConcurrentHandshakes, 1, MaxConcurrentHandshakeCap)

	if len(rt.Certificates) == 0 {
		v.add(path+".certificates", "at least one certificate is required")
	}
	if len(rt.Certificates) > MaxCertificates {
		v.add(path+".certificates", "at most %d certificates", MaxCertificates)
	}
	for i, rc := range rt.Certificates {
		cp := fmt.Sprintf("%s.certificates[%d]", path, i)
		c := Certificate{CertFile: rc.CertFile, KeyFile: rc.KeyFile}
		if c.CertFile == "" {
			v.add(cp+".cert_file", "is required")
		}
		if c.KeyFile == "" {
			v.add(cp+".key_file", "is required")
		}
		if rc.ServerNames != nil && len(rc.ServerNames) == 0 {
			v.add(cp+".server_names", "must not be empty; omit it to use the certificate's names")
		}
		if len(rc.ServerNames) > MaxServerNamesPerCert {
			v.add(cp+".server_names", "at most %d names", MaxServerNamesPerCert)
		}
		for j, n := range rc.ServerNames {
			n = strings.ToLower(n)
			if n == "*" || checkHostPattern(n) != nil {
				v.add(fmt.Sprintf("%s.server_names[%d]", cp, j), "%q must be a host name or *.suffix", n)
				continue
			}
			c.ServerNames = append(c.ServerNames, n)
		}
		t.Certificates = append(t.Certificates, c)
	}
	return t
}

func (v *validator) cipherSuites(path string, names []string) []uint16 {
	if len(names) == 0 {
		v.add(path, "must not be empty")
		return nil
	}
	byName := map[string]*tls.CipherSuite{}
	for _, cs := range tls.CipherSuites() {
		byName[cs.Name] = cs
	}
	var ids []uint16
	seen := map[uint16]bool{}
	for i, n := range names {
		cs := byName[n]
		switch {
		case cs == nil:
			v.add(fmt.Sprintf("%s[%d]", path, i), "%q is not a supported secure cipher suite", n)
		case !supportsTLS12(cs):
			v.add(fmt.Sprintf("%s[%d]", path, i), "%q is a TLS 1.3 suite; those are always enabled", n)
		case seen[cs.ID]:
			v.add(fmt.Sprintf("%s[%d]", path, i), "duplicate cipher suite %q", n)
		default:
			seen[cs.ID] = true
			ids = append(ids, cs.ID)
		}
	}
	return ids
}

func supportsTLS12(cs *tls.CipherSuite) bool {
	for _, ver := range cs.SupportedVersions {
		if ver == tls.VersionTLS12 {
			return true
		}
	}
	return false
}

func (v *validator) cluster(path string, rc *rawCluster) Cluster {
	c := Cluster{Name: rc.Name, Policy: Policy(rc.Policy)}
	if !validName(c.Name) {
		v.add(path+".name", "must be 1 to 64 characters from A-Z a-z 0-9 . _ -")
	}
	switch c.Policy {
	case "":
		c.Policy = RoundRobin
	case RoundRobin, LeastLoaded, Maglev:
	default:
		v.add(path+".policy", "must be round_robin, least_loaded or maglev")
	}
	if c.Policy == Maglev {
		if !validToken(rc.HashHeader) {
			v.add(path+".hash_header", "is required for maglev and must be a valid header name")
		} else {
			c.HashHeader = textproto.CanonicalMIMEHeaderKey(rc.HashHeader)
		}
	} else if rc.HashHeader != "" {
		v.add(path+".hash_header", "is only used with the maglev policy")
	}

	if len(rc.Endpoints) == 0 {
		v.add(path+".endpoints", "at least one endpoint is required")
	}
	if len(rc.Endpoints) > MaxEndpoints {
		v.add(path+".endpoints", "at most %d endpoints", MaxEndpoints)
	}
	addrs := map[string]bool{}
	for i, re := range rc.Endpoints {
		ep := fmt.Sprintf("%s.endpoints[%d]", path, i)
		e := Endpoint{Address: re.Address}
		if err := checkEndpointAddress(e.Address); err != nil {
			v.add(ep+".address", "%v", err)
		}
		if addrs[e.Address] {
			v.add(ep+".address", "duplicate address %q", e.Address)
		}
		addrs[e.Address] = true
		e.Weight = v.intIn(ep+".weight", re.Weight, 1, 1, MaxWeight)
		e.Priority = v.intIn(ep+".priority", re.Priority, 0, 0, MaxPriority)
		c.Endpoints = append(c.Endpoints, e)
	}

	c.ConnectTimeout = v.durIn(path+".connect_timeout", rc.ConnectTimeout, DefaultConnectTimeout, time.Millisecond, time.Minute)
	c.Timeout = v.durIn(path+".timeout", rc.Timeout, DefaultTimeout, 0, time.Hour)
	c.MaxRequests = v.intIn(path+".max_requests", rc.MaxRequests, DefaultMaxRequests, 1, 1_000_000)
	c.PanicThresholdPercent = v.intIn(path+".panic_threshold_percent", rc.PanicThresholdPercent, DefaultPanicThresholdPercent, 0, 100)

	if h := rc.HealthCheck; h != nil {
		hp := path + ".health_check"
		hc := &HealthCheck{Path: h.Path, Host: h.Host}
		if err := checkProbePath(h.Path); err != nil {
			v.add(hp+".path", "%v", err)
		}
		if h.Host != "" && checkEndpointAddress(h.Host) != nil && checkHostname(h.Host) != nil {
			v.add(hp+".host", "must be a host name or host:port")
		}
		hc.Interval = v.durIn(hp+".interval", h.Interval, DefaultHealthInterval, 10*time.Millisecond, time.Hour)
		hc.Jitter = v.durIn(hp+".jitter", h.Jitter, 0, 0, time.Hour)
		hc.Timeout = v.durIn(hp+".timeout", h.Timeout, min(DefaultHealthTimeout, hc.Interval), time.Millisecond, time.Hour)
		if hc.Timeout > hc.Interval {
			v.add(hp+".timeout", "must not exceed interval")
		}
		if hc.Jitter > hc.Interval {
			v.add(hp+".jitter", "must not exceed interval")
		}
		hc.HealthyThreshold = v.intIn(hp+".healthy_threshold", h.HealthyThreshold, DefaultHealthyAfter, 1, 100)
		hc.UnhealthyThreshold = v.intIn(hp+".unhealthy_threshold", h.UnhealthyThreshold, DefaultUnhealthyAfter, 1, 100)
		c.HealthCheck = hc
	}

	if o := rc.OutlierDetection; o != nil {
		op := path + ".outlier_detection"
		od := &OutlierDetection{}
		od.ConsecutiveGatewayFailures = v.intIn(op+".consecutive_gateway_failures", o.ConsecutiveGatewayFailures, DefaultConsecutiveGatewayFailures, 0, 1000)
		od.Interval = v.durIn(op+".interval", o.Interval, DefaultOutlierInterval, 10*time.Millisecond, time.Hour)
		od.BaseEjectionTime = v.durIn(op+".base_ejection_time", o.BaseEjectionTime, DefaultBaseEjectionTime, time.Millisecond, time.Hour)
		od.MaxEjectionTime = v.durIn(op+".max_ejection_time", o.MaxEjectionTime, max(DefaultMaxEjectionTime, od.BaseEjectionTime), time.Millisecond, 24*time.Hour)
		if od.MaxEjectionTime < od.BaseEjectionTime {
			v.add(op+".max_ejection_time", "must not be less than base_ejection_time")
		}
		od.MaxEjectionPercent = v.intIn(op+".max_ejection_percent", o.MaxEjectionPercent, DefaultMaxEjectionPercent, 0, 100)
		od.SuccessRateMinimumHosts = v.intIn(op+".success_rate_minimum_hosts", o.SuccessRateMinimumHosts, DefaultSuccessRateMinimumHosts, 0, MaxEndpoints)
		od.SuccessRateRequestVolume = v.intIn(op+".success_rate_request_volume", o.SuccessRateRequestVolume, DefaultSuccessRateRequestVolume, 1, 1_000_000)
		od.SuccessRateStdevFactor = DefaultSuccessRateStdevFactor
		if f := o.SuccessRateStdevFactor; f != nil {
			if !(*f > 0 && *f <= 10) {
				v.add(op+".success_rate_stdev_factor", "must be greater than 0 and at most 10")
			}
			od.SuccessRateStdevFactor = *f
		}
		c.OutlierDetection = od
	}

	if r := rc.Retry; r != nil {
		rp := path + ".retry"
		rt := &Retry{}
		rt.MaxRetries = v.intIn(rp+".max_retries", r.MaxRetries, DefaultMaxRetries, 0, MaxRetries)
		if r.RetryOn == nil {
			rt.RetryOn = append([]int(nil), DefaultRetryOn...)
		} else {
			seenStatus := map[int]bool{}
			for i, s := range r.RetryOn {
				if s < 500 || s > 599 {
					v.add(fmt.Sprintf("%s.retry_on[%d]", rp, i), "must be a 5xx status")
				}
				if seenStatus[s] {
					v.add(fmt.Sprintf("%s.retry_on[%d]", rp, i), "duplicate status %d", s)
				}
				seenStatus[s] = true
				rt.RetryOn = append(rt.RetryOn, s)
			}
		}
		rt.PerTryTimeout = v.durIn(rp+".per_try_timeout", r.PerTryTimeout, 0, 0, time.Hour)
		if c.Timeout > 0 && rt.PerTryTimeout > c.Timeout {
			v.add(rp+".per_try_timeout", "must not exceed the cluster timeout")
		}
		rt.BackoffBase = v.durIn(rp+".backoff_base", r.BackoffBase, DefaultBackoffBase, time.Millisecond, time.Minute)
		rt.BackoffMax = v.durIn(rp+".backoff_max", r.BackoffMax, max(DefaultBackoffMax, rt.BackoffBase), time.Millisecond, time.Minute)
		if rt.BackoffMax < rt.BackoffBase {
			v.add(rp+".backoff_max", "must not be less than backoff_base")
		}
		rt.MaxBodyBytes = DefaultRetryBodyBytes
		if b := r.MaxBodyBytes; b != nil {
			if *b < 0 || *b > MaxRetryBodyBytes {
				v.add(rp+".max_body_bytes", "must be between 0 and %d", MaxRetryBodyBytes)
			}
			rt.MaxBodyBytes = *b
		}
		rt.Budget = Budget{Ratio: DefaultBudgetRatio, MinPerSecond: DefaultBudgetMinPerSec, Window: DefaultBudgetWindow}
		if b := r.Budget; b != nil {
			bp := rp + ".budget"
			if b.Ratio != nil {
				if !(*b.Ratio >= 0 && *b.Ratio <= 1) {
					v.add(bp+".ratio", "must be between 0 and 1")
				}
				rt.Budget.Ratio = *b.Ratio
			}
			rt.Budget.MinPerSecond = v.intIn(bp+".min_per_second", b.MinPerSecond, DefaultBudgetMinPerSec, 0, 100_000)
			rt.Budget.Window = v.durIn(bp+".window", b.Window, DefaultBudgetWindow, time.Second, time.Minute)
			if rt.Budget.Window%time.Second != 0 {
				v.add(bp+".window", "must be a whole number of seconds")
			}
		}
		c.Retry = rt
	}
	return c
}

func validName(s string) bool {
	if len(s) == 0 || len(s) > 64 {
		return false
	}
	for i := 0; i < len(s); i++ {
		ch := s[i]
		if !(ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' || ch == '.' || ch == '_' || ch == '-') {
			return false
		}
	}
	return true
}

// validToken reports whether s is an RFC 9110 token of at most 64 bytes.
func validToken(s string) bool {
	if len(s) == 0 || len(s) > 64 {
		return false
	}
	for i := 0; i < len(s); i++ {
		ch := s[i]
		if ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' {
			continue
		}
		if !strings.ContainsRune("!#$%&'*+-.^_`|~", rune(ch)) {
			return false
		}
	}
	return true
}

func checkListen(addr string) error {
	host, port, err := net.SplitHostPort(addr)
	if err != nil {
		return fmt.Errorf("%q is not host:port", addr)
	}
	if err := checkPort(port, true); err != nil {
		return err
	}
	if host == "" || host == "localhost" {
		return nil
	}
	if _, err := netip.ParseAddr(host); err != nil {
		return fmt.Errorf("listen host %q must be an IP address, localhost or empty", host)
	}
	return nil
}

func checkPort(port string, allowZero bool) error {
	n, err := strconv.Atoi(port)
	if err != nil || n < 0 || n > 65535 || (n == 0 && !allowZero) || strconv.Itoa(n) != port {
		return fmt.Errorf("port %q must be a number between 1 and 65535", port)
	}
	return nil
}

func checkEndpointAddress(addr string) error {
	host, port, err := net.SplitHostPort(addr)
	if err != nil {
		return fmt.Errorf("%q is not host:port", addr)
	}
	if err := checkPort(port, false); err != nil {
		return err
	}
	if _, err := netip.ParseAddr(host); err == nil {
		return nil
	}
	if err := checkHostname(host); err != nil {
		return err
	}
	return nil
}

// checkHostname accepts DNS names made of letters, digits and hyphens.
func checkHostname(h string) error {
	if len(h) == 0 || len(h) > 253 {
		return fmt.Errorf("host %q must be 1 to 253 characters", h)
	}
	for _, label := range strings.Split(h, ".") {
		if len(label) == 0 || len(label) > 63 || label[0] == '-' || label[len(label)-1] == '-' {
			return fmt.Errorf("host %q is not a valid DNS name", h)
		}
		for i := 0; i < len(label); i++ {
			ch := label[i]
			if !(ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' || ch == '-') {
				return fmt.Errorf("host %q is not a valid DNS name", h)
			}
		}
	}
	return nil
}

func checkHostPattern(h string) error {
	if h == "*" {
		return nil
	}
	if rest, ok := strings.CutPrefix(h, "*."); ok {
		if checkHostname(rest) != nil {
			return fmt.Errorf("%q must be *, a host name, or *.suffix", h)
		}
		return nil
	}
	if checkHostname(h) != nil {
		return fmt.Errorf("%q must be *, a host name, or *.suffix (no port)", h)
	}
	return nil
}

// checkPathPrefix requires an absolute path without empty, "." or ".."
// segments, so the prefix can be compared with cleaned request paths.
func checkPathPrefix(p string) error {
	if len(p) > MaxPathPrefixLen {
		return fmt.Errorf("must be at most %d characters", MaxPathPrefixLen)
	}
	if !strings.HasPrefix(p, "/") {
		return errors.New(`must start with "/"`)
	}
	for i := 0; i < len(p); i++ {
		if p[i] <= ' ' || p[i] == 0x7f || p[i] == '?' || p[i] == '#' || p[i] == '%' || p[i] == '\\' {
			return errors.New("must not contain spaces, control characters, ?, #, % or backslashes")
		}
	}
	segs := strings.Split(p[1:], "/")
	for i, s := range segs {
		if s == "." || s == ".." || (s == "" && i != len(segs)-1) {
			return errors.New(`must not contain empty, "." or ".." segments`)
		}
	}
	return nil
}

func checkProbePath(p string) error {
	if !strings.HasPrefix(p, "/") || len(p) > MaxPathPrefixLen {
		return fmt.Errorf(`must start with "/" and be at most %d characters`, MaxPathPrefixLen)
	}
	for i := 0; i < len(p); i++ {
		if p[i] <= ' ' || p[i] == 0x7f || p[i] == '#' {
			return errors.New("must not contain spaces, control characters or #")
		}
	}
	return nil
}

func parsePrefix(s string) (netip.Prefix, error) {
	if p, err := netip.ParsePrefix(s); err == nil {
		if p != p.Masked() {
			return netip.Prefix{}, fmt.Errorf("%q has host bits set; did you mean %s?", s, p.Masked())
		}
		return p, nil
	}
	if a, err := netip.ParseAddr(s); err == nil && a.Zone() == "" {
		return netip.PrefixFrom(a, a.BitLen()), nil
	}
	return netip.Prefix{}, fmt.Errorf("%q is not an IP address or CIDR", s)
}
