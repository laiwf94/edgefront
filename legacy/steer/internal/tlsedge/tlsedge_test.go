package tlsedge

import (
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"errors"
	"io"
	"math/big"
	"net"
	"net/http"
	"net/http/httptrace"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/laiwf94/steer/internal/config"
)

// testCA signs leaf certificates for the tests.
type testCA struct {
	cert *x509.Certificate
	key  crypto.Signer
	pool *x509.CertPool
}

func newCA(t testing.TB) *testCA {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	tmpl := &x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: "steer test CA"},
		NotBefore:             time.Now().Add(-time.Hour),
		NotAfter:              time.Now().Add(24 * time.Hour),
		IsCA:                  true,
		BasicConstraintsValid: true,
		KeyUsage:              x509.KeyUsageCertSign,
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, key.Public(), key)
	if err != nil {
		t.Fatal(err)
	}
	cert, _ := x509.ParseCertificate(der)
	pool := x509.NewCertPool()
	pool.AddCert(cert)
	return &testCA{cert: cert, key: key, pool: pool}
}

var serial atomic.Int64

type leafOpts struct {
	rsa       bool
	notBefore time.Time
	notAfter  time.Time
}

// issue writes a certificate for names and its key to dir and returns the
// two paths.
func (ca *testCA) issue(t testing.TB, dir, base string, names []string, o leafOpts) (string, string) {
	t.Helper()
	var key crypto.Signer
	var err error
	if o.rsa {
		key, err = rsa.GenerateKey(rand.Reader, 2048)
	} else {
		key, err = ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	}
	if err != nil {
		t.Fatal(err)
	}
	if o.notBefore.IsZero() {
		o.notBefore = time.Now().Add(-time.Hour)
	}
	if o.notAfter.IsZero() {
		o.notAfter = time.Now().Add(12 * time.Hour)
	}
	tmpl := &x509.Certificate{
		SerialNumber: big.NewInt(serial.Add(1) + 1),
		Subject:      pkix.Name{CommonName: base},
		DNSNames:     names,
		NotBefore:    o.notBefore,
		NotAfter:     o.notAfter,
		KeyUsage:     x509.KeyUsageDigitalSignature | x509.KeyUsageKeyEncipherment,
		ExtKeyUsage:  []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, ca.cert, key.Public(), ca.key)
	if err != nil {
		t.Fatal(err)
	}
	keyDER, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	certPath := filepath.Join(dir, base+".crt")
	keyPath := filepath.Join(dir, base+".key")
	write(t, certPath, pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}))
	write(t, keyPath, pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: keyDER}))
	return certPath, keyPath
}

func write(t testing.TB, path string, b []byte) {
	t.Helper()
	if err := os.WriteFile(path, b, 0o600); err != nil {
		t.Fatal(err)
	}
}

func tlsConfig(certs ...config.Certificate) *config.TLS {
	return &config.TLS{Certificates: certs, MinVersion: tls.VersionTLS12,
		HandshakeTimeout: 2 * time.Second, MaxConcurrentHandshakes: 64}
}

func cert(c, k string, names ...string) config.Certificate {
	return config.Certificate{CertFile: c, KeyFile: k, ServerNames: names}
}

func mustLoad(t testing.TB, c *config.TLS) *Snapshot {
	t.Helper()
	s, err := Load(c)
	if err != nil {
		t.Fatal(err)
	}
	return s
}

// serve runs an HTTPS server that answers with the request's server name on
// a Listener over loopback.
func serve(t testing.TB, st *Store, onHandshake ...func(string, time.Duration)) *Listener {
	t.Helper()
	inner, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	var fn func(string, time.Duration)
	if len(onHandshake) > 0 {
		fn = onHandshake[0]
	}
	l := NewListener(inner, st, fn)
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, r.TLS.ServerName)
	})}
	go srv.Serve(l)
	t.Cleanup(func() {
		srv.Close()
		l.Close()
		l.Wait()
	})
	return l
}

// handshake connects to addr with SNI name and returns the leaf the server
// presented, or the handshake error.
func handshake(t testing.TB, addr, name string, ca *testCA, tweak func(*tls.Config)) (*tls.ConnectionState, error) {
	t.Helper()
	cfg := &tls.Config{ServerName: name, RootCAs: ca.pool}
	if name == "" {
		// No SNI: verify by hand against the CA only.
		cfg.InsecureSkipVerify = true
	}
	if tweak != nil {
		tweak(cfg)
	}
	d := &net.Dialer{Timeout: 5 * time.Second}
	c, err := tls.DialWithDialer(d, "tcp", addr, cfg)
	if err != nil {
		return nil, err
	}
	defer c.Close()
	cs := c.ConnectionState()
	return &cs, nil
}

type fixture struct {
	ca    *testCA
	dir   string
	paths map[string][2]string
}

func newFixture(t testing.TB) *fixture {
	t.Helper()
	f := &fixture{ca: newCA(t), dir: t.TempDir(), paths: map[string][2]string{}}
	add := func(base string, names []string, o leafOpts) {
		c, k := f.ca.issue(t, f.dir, base, names, o)
		f.paths[base] = [2]string{c, k}
	}
	add("default", []string{"default.test"}, leafOpts{})
	add("exact", []string{"api.example.com"}, leafOpts{})
	add("wild", []string{"*.example.com", "example.com"}, leafOpts{})
	add("dual-ecdsa", []string{"dual.test"}, leafOpts{})
	add("dual-rsa", []string{"dual.test"}, leafOpts{rsa: true})
	return f
}

func (f *fixture) cert(base string, names ...string) config.Certificate {
	p := f.paths[base]
	return cert(p[0], p[1], names...)
}

func (f *fixture) standard() *config.TLS {
	return tlsConfig(f.cert("default"), f.cert("exact"), f.cert("wild"), f.cert("dual-ecdsa"), f.cert("dual-rsa"))
}

func TestSelect(t *testing.T) {
	f := newFixture(t)
	s := mustLoad(t, f.standard())
	cases := []struct{ sni, want string }{
		{"api.example.com", "exact"},
		{"API.Example.COM.", "exact"},
		{"www.example.com", "wild"},
		{"example.com", "wild"}, // listed as an exact name on the wildcard certificate
		{"a.b.example.com", "default"},
		{"example.com.evil", "default"},
		{".example.com", "default"},
		{"", "default"},
		{"unknown.test", "default"},
		{"default.test", "default"},
		{"dual.test", "dual-ecdsa"},
	}
	for _, c := range cases {
		got, err := s.Select(c.sni, nil)
		if err != nil {
			t.Fatalf("Select(%q): %v", c.sni, err)
		}
		if cn := got.Leaf.Subject.CommonName; cn != c.want {
			t.Errorf("Select(%q) = %s, want %s", c.sni, cn, c.want)
		}
	}

	strict := f.standard()
	strict.StrictSNI = true
	s = mustLoad(t, strict)
	for _, sni := range []string{"", "a.b.example.com", "unknown.test"} {
		if _, err := s.Select(sni, nil); !errors.Is(err, ErrUnknownServerName) {
			t.Errorf("strict Select(%q) error = %v, want ErrUnknownServerName", sni, err)
		}
	}
	if got, err := s.Select("default.test", nil); err != nil || got.Leaf.Subject.CommonName != "default" {
		t.Errorf("strict Select(default.test) = %v, %v", got, err)
	}
}

func TestServerNamesOverride(t *testing.T) {
	f := newFixture(t)
	s := mustLoad(t, tlsConfig(f.cert("default"), f.cert("wild", "*.example.org")))
	if got, _ := s.Select("www.example.com", nil); got.Leaf.Subject.CommonName != "default" {
		t.Errorf("www.example.com chose %s; server_names should replace the certificate's own names", got.Leaf.Subject.CommonName)
	}
	if got, _ := s.Select("www.example.org", nil); got.Leaf.Subject.CommonName != "wild" {
		t.Errorf("www.example.org chose %s, want wild", got.Leaf.Subject.CommonName)
	}
	w := s.Warnings(time.Now())
	if len(w) != 1 || !strings.Contains(w[0], `"*.example.org"`) {
		t.Errorf("warnings = %q, want one about *.example.org not being covered", w)
	}
}

func TestWarningsValidity(t *testing.T) {
	ca := newCA(t)
	dir := t.TempDir()
	ec, ek := ca.issue(t, dir, "expired", []string{"old.test"}, leafOpts{notBefore: time.Now().Add(-48 * time.Hour), notAfter: time.Now().Add(-time.Hour)})
	fc, fk := ca.issue(t, dir, "future", []string{"new.test"}, leafOpts{notBefore: time.Now().Add(time.Hour), notAfter: time.Now().Add(48 * time.Hour)})
	s := mustLoad(t, tlsConfig(cert(ec, ek), cert(fc, fk)))
	w := strings.Join(s.Warnings(time.Now()), "\n")
	if !strings.Contains(w, "expired.crt expired") || !strings.Contains(w, "future.crt is not valid until") {
		t.Errorf("warnings = %s", w)
	}
	if n := len(s.Certificates()); n != 2 {
		t.Errorf("Certificates() has %d entries, want 2", n)
	}
}

func TestLoadErrors(t *testing.T) {
	f := newFixture(t)
	dir := t.TempDir()
	garbage := filepath.Join(dir, "garbage.pem")
	write(t, garbage, []byte("not a certificate"))
	noNames, noNamesKey := f.ca.issue(t, dir, "nonames", nil, leafOpts{})
	cases := map[string]*config.TLS{
		"nil":            nil,
		"no certs":       tlsConfig(),
		"missing file":   tlsConfig(cert(filepath.Join(dir, "absent.crt"), f.paths["default"][1])),
		"garbage":        tlsConfig(cert(garbage, f.paths["default"][1])),
		"key mismatch":   tlsConfig(cert(f.paths["default"][0], f.paths["exact"][1])),
		"later no names": tlsConfig(f.cert("default"), cert(noNames, noNamesKey)),
	}
	for name, c := range cases {
		if _, err := Load(c); err == nil {
			t.Errorf("%s: Load succeeded", name)
		}
	}
	// A first certificate without names is fine: it is the fallback.
	if _, err := Load(tlsConfig(cert(noNames, noNamesKey))); err != nil {
		t.Errorf("nameless fallback: %v", err)
	}
}

// TestHandshakeMatrix drives real handshakes over loopback.
func TestHandshakeMatrix(t *testing.T) {
	f := newFixture(t)
	st := NewStore(mustLoad(t, f.standard()))
	l := serve(t, st)
	addr := l.Addr().String()

	for _, c := range []struct{ sni, want string }{
		{"api.example.com", "exact"}, {"www.example.com", "wild"}, {"default.test", "default"},
	} {
		cs, err := handshake(t, addr, c.sni, f.ca, func(c *tls.Config) { c.NextProtos = []string{"h2", "http/1.1"} })
		if err != nil {
			t.Fatalf("%s: %v", c.sni, err)
		}
		if cn := cs.PeerCertificates[0].Subject.CommonName; cn != c.want {
			t.Errorf("%s: got %s, want %s", c.sni, cn, c.want)
		}
		if cs.Version != tls.VersionTLS13 || cs.NegotiatedProtocol != "http/1.1" {
			t.Errorf("%s: version %x, ALPN %q", c.sni, cs.Version, cs.NegotiatedProtocol)
		}
	}
	// A client that only takes RSA gets the RSA certificate of a dual pair.
	cs, err := handshake(t, addr, "dual.test", f.ca, func(c *tls.Config) {
		c.MaxVersion = tls.VersionTLS12
		c.CipherSuites = []uint16{tls.TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256}
	})
	if err != nil {
		t.Fatal(err)
	}
	if cn := cs.PeerCertificates[0].Subject.CommonName; cn != "dual-rsa" {
		t.Errorf("RSA-only client got %s", cn)
	}
	// Unknown names fall back to the default, which the client then rejects.
	if _, err := handshake(t, addr, "a.b.example.com", f.ca, nil); err == nil {
		t.Error("a.b.example.com verified against the default certificate")
	}
	// Plain TLS 1.2 still works with the modern policy.
	if cs, err := handshake(t, addr, "api.example.com", f.ca, func(c *tls.Config) { c.MaxVersion = tls.VersionTLS12 }); err != nil || cs.Version != tls.VersionTLS12 {
		t.Errorf("TLS 1.2 client: %v", err)
	}
}

func TestVersionAndCipherPolicy(t *testing.T) {
	f := newFixture(t)
	only13 := f.standard()
	only13.MinVersion = tls.VersionTLS13
	only13.CipherSuites = nil
	st := NewStore(mustLoad(t, only13))
	addr := serve(t, st).Addr().String()
	if _, err := handshake(t, addr, "api.example.com", f.ca, func(c *tls.Config) { c.MaxVersion = tls.VersionTLS12 }); err == nil {
		t.Error("TLS 1.2 client accepted with min_version 1.3")
	}
	if _, err := handshake(t, addr, "api.example.com", f.ca, nil); err != nil {
		t.Errorf("TLS 1.3 client: %v", err)
	}

	cbcOnly := func(c *tls.Config) {
		c.MaxVersion = tls.VersionTLS12
		c.CipherSuites = []uint16{tls.TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA}
	}
	modern := f.standard()
	modern.CipherSuites = []uint16{tls.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256}
	st.Swap(mustLoad(t, modern))
	if _, err := handshake(t, addr, "api.example.com", f.ca, cbcOnly); err == nil {
		t.Error("CBC-only client accepted by a GCM-only policy")
	}
	compat := f.standard()
	compat.CipherSuites = []uint16{tls.TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, tls.TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA}
	st.Swap(mustLoad(t, compat))
	cs, err := handshake(t, addr, "api.example.com", f.ca, cbcOnly)
	if err != nil {
		t.Fatalf("CBC-only client with a CBC suite allowed: %v", err)
	}
	if cs.CipherSuite != tls.TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA {
		t.Errorf("negotiated %s", tls.CipherSuiteName(cs.CipherSuite))
	}
}

func TestStrictSNIHandshake(t *testing.T) {
	f := newFixture(t)
	c := f.standard()
	c.StrictSNI = true
	st := NewStore(mustLoad(t, c))
	var results sync.Map
	l := serve(t, st, func(r string, _ time.Duration) {
		n, _ := results.LoadOrStore(r, new(atomic.Int64))
		n.(*atomic.Int64).Add(1)
	})
	addr := l.Addr().String()
	if _, err := handshake(t, addr, "unknown.test", f.ca, nil); err == nil {
		t.Error("unknown SNI accepted under strict_sni")
	}
	if _, err := handshake(t, addr, "", f.ca, nil); err == nil {
		t.Error("missing SNI accepted under strict_sni")
	}
	if _, err := handshake(t, addr, "api.example.com", f.ca, nil); err != nil {
		t.Errorf("known SNI: %v", err)
	}
	waitFor(t, func() bool { return count(&results, ResultError) == 2 && count(&results, ResultOK) == 1 })
}

func count(m *sync.Map, k string) int64 {
	v, ok := m.Load(k)
	if !ok {
		return 0
	}
	return v.(*atomic.Int64).Load()
}

func waitFor(t testing.TB, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatal("condition not met within 5s")
		}
		time.Sleep(5 * time.Millisecond)
	}
}

// TestHandshakeTimeout checks that a client that connects and sends nothing
// is cut off at the handshake timeout, and does not delay other clients.
func TestHandshakeTimeout(t *testing.T) {
	f := newFixture(t)
	c := f.standard()
	c.HandshakeTimeout = 300 * time.Millisecond
	st := NewStore(mustLoad(t, c))
	var timeouts atomic.Int64
	l := serve(t, st, func(r string, _ time.Duration) {
		if r == ResultTimeout {
			timeouts.Add(1)
		}
	})
	addr := l.Addr().String()

	var stalled []net.Conn
	for range 8 {
		raw, err := net.Dial("tcp", addr)
		if err != nil {
			t.Fatal(err)
		}
		defer raw.Close()
		stalled = append(stalled, raw)
	}
	// A real client gets through at once while the stalled ones wait.
	start := time.Now()
	if _, err := handshake(t, addr, "api.example.com", f.ca, nil); err != nil {
		t.Fatal(err)
	}
	if d := time.Since(start); d > 250*time.Millisecond {
		t.Errorf("handshake took %v behind stalled connections", d)
	}
	// Each stalled connection is closed by the server around the timeout.
	for _, raw := range stalled {
		raw.SetReadDeadline(time.Now().Add(5 * time.Second))
		_, err := raw.Read(make([]byte, 1))
		if err == nil || errors.Is(err, os.ErrDeadlineExceeded) {
			t.Fatalf("stalled connection: read returned %v, want the server to close it", err)
		}
	}
	if d := time.Since(start); d < 250*time.Millisecond {
		t.Errorf("stalled connections closed after %v, before the 300ms timeout", d)
	}
	waitFor(t, func() bool { return timeouts.Load() == 8 })
	waitFor(t, func() bool { return l.InProgress() == 0 })
}

// TestMaxConcurrentHandshakes checks that once the limit is reached new
// connections wait for a slot instead of starting a handshake.
func TestMaxConcurrentHandshakes(t *testing.T) {
	f := newFixture(t)
	c := f.standard()
	c.HandshakeTimeout = 400 * time.Millisecond
	c.MaxConcurrentHandshakes = 2
	st := NewStore(mustLoad(t, c))
	l := serve(t, st)
	addr := l.Addr().String()
	for range 2 {
		raw, err := net.Dial("tcp", addr)
		if err != nil {
			t.Fatal(err)
		}
		defer raw.Close()
	}
	waitFor(t, func() bool { return l.InProgress() == 2 })
	start := time.Now()
	if _, err := handshake(t, addr, "api.example.com", f.ca, nil); err != nil {
		t.Fatal(err)
	}
	if d := time.Since(start); d < 300*time.Millisecond {
		t.Errorf("third handshake finished after %v; it should have waited for a stalled one to time out", d)
	}
}

// TestReloadKeepsLiveConnections swaps the certificate under a keep-alive
// connection: the connection keeps working with the old certificate and new
// connections get the new one.
func TestReloadKeepsLiveConnections(t *testing.T) {
	f := newFixture(t)
	st := NewStore(mustLoad(t, tlsConfig(f.cert("exact"))))
	addr := serve(t, st).Addr().String()

	tr := &http.Transport{TLSClientConfig: &tls.Config{RootCAs: f.ca.pool, ServerName: "api.example.com"}}
	defer tr.CloseIdleConnections()
	client := &http.Client{Transport: tr, Timeout: 5 * time.Second}
	get := func() (cn string, reused bool) {
		t.Helper()
		var info httptrace.GotConnInfo
		ctx := httptrace.WithClientTrace(context.Background(), &httptrace.ClientTrace{GotConn: func(i httptrace.GotConnInfo) { info = i }})
		req, _ := http.NewRequestWithContext(ctx, http.MethodGet, "https://"+addr+"/", nil)
		resp, err := client.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
		return resp.TLS.PeerCertificates[0].Subject.CommonName, info.Reused
	}
	if cn, _ := get(); cn != "exact" {
		t.Fatalf("first request served by %s", cn)
	}

	// Rotate to a new certificate for the same name.
	c, k := f.ca.issue(t, f.dir, "rotated", []string{"api.example.com"}, leafOpts{})
	st.Swap(mustLoad(t, tlsConfig(cert(c, k))))

	for range 3 {
		cn, reused := get()
		if !reused || cn != "exact" {
			t.Fatalf("after reload: reused=%v cert=%s; the live connection should carry on", reused, cn)
		}
	}
	tr.CloseIdleConnections()
	if cn, reused := get(); reused || cn != "rotated" {
		t.Fatalf("new connection: reused=%v cert=%s, want the rotated certificate", reused, cn)
	}
}

// TestReloadUnderLoad swaps snapshots continuously while clients make new
// connections; no handshake or request may fail.
func TestReloadUnderLoad(t *testing.T) {
	f := newFixture(t)
	a := mustLoad(t, f.standard())
	b := mustLoad(t, tlsConfig(f.cert("wild"), f.cert("exact")))
	st := NewStore(a)
	addr := serve(t, st).Addr().String()

	stop := make(chan struct{})
	var swaps sync.WaitGroup
	swaps.Add(1)
	go func() {
		defer swaps.Done()
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			default:
			}
			if i%2 == 0 {
				st.Swap(b)
			} else {
				st.Swap(a)
			}
			time.Sleep(100 * time.Microsecond)
		}
	}()
	var wg sync.WaitGroup
	var failures atomic.Int64
	for range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			tr := &http.Transport{TLSClientConfig: &tls.Config{RootCAs: f.ca.pool, ServerName: "api.example.com"}, DisableKeepAlives: true}
			client := &http.Client{Transport: tr, Timeout: 5 * time.Second}
			for range 25 {
				resp, err := client.Get("https://" + addr + "/")
				if err != nil {
					failures.Add(1)
					t.Log(err)
					continue
				}
				io.Copy(io.Discard, resp.Body)
				resp.Body.Close()
			}
		}()
	}
	wg.Wait()
	close(stop)
	swaps.Wait()
	if n := failures.Load(); n != 0 {
		t.Fatalf("%d of 200 requests failed during reloads", n)
	}
}

func TestListenerClose(t *testing.T) {
	f := newFixture(t)
	st := NewStore(mustLoad(t, f.standard()))
	inner, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	l := NewListener(inner, st, nil)
	errc := make(chan error, 1)
	go func() {
		_, err := l.Accept()
		errc <- err
	}()
	raw, err := net.Dial("tcp", l.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer raw.Close()
	waitFor(t, func() bool { return l.InProgress() == 1 })
	l.Close()
	if err := <-errc; !errors.Is(err, net.ErrClosed) {
		t.Fatalf("Accept after Close = %v, want net.ErrClosed", err)
	}
	l.Wait() // the pending handshake must give up
	raw.SetReadDeadline(time.Now().Add(5 * time.Second))
	if _, err := raw.Read(make([]byte, 1)); err == nil || errors.Is(err, os.ErrDeadlineExceeded) {
		t.Fatalf("pending handshake's connection still open after Close: %v", err)
	}
	if _, err := l.Accept(); !errors.Is(err, net.ErrClosed) {
		t.Fatalf("second Accept = %v", err)
	}
}

// failingListener returns a permanent error from Accept.
type failingListener struct{ net.Listener }

func (failingListener) Accept() (net.Conn, error) { return nil, errors.New("listener broken") }

func TestInnerAcceptError(t *testing.T) {
	f := newFixture(t)
	inner, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer inner.Close()
	l := NewListener(failingListener{inner}, NewStore(mustLoad(t, f.standard())), nil)
	if _, err := l.Accept(); err == nil || err.Error() != "listener broken" {
		t.Fatalf("Accept = %v, want the inner error", err)
	}
	l.Close()
	l.Wait()
}

func BenchmarkSelect(b *testing.B) {
	f := newFixture(b)
	c := tlsConfig(f.cert("default"))
	for i := range 200 {
		base := "host" + string(rune('a'+i%26)) + string(rune('a'+i/26))
		cp, kp := f.ca.issue(b, f.dir, base, []string{base + ".example.net", "*." + base + ".example.org"}, leafOpts{})
		c.Certificates = append(c.Certificates, cert(cp, kp))
	}
	s := mustLoad(b, c)
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if _, err := s.Select("www.hostqg.example.org", nil); err != nil {
			b.Fatal(err)
		}
	}
}

func BenchmarkHandshake(b *testing.B) {
	f := newFixture(b)
	st := NewStore(mustLoad(b, f.standard()))
	addr := serve(b, st).Addr().String()
	cfg := &tls.Config{RootCAs: f.ca.pool, ServerName: "api.example.com"}
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		c, err := tls.Dial("tcp", addr, cfg)
		if err != nil {
			b.Fatal(err)
		}
		c.Close()
	}
}
