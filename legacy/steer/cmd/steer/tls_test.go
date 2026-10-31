package main

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/http/httptrace"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// writeCert issues a certificate for name signed by caKey/caCert and writes
// it and its key to dir/base.crt and dir/base.key.
func writeCert(t *testing.T, dir, base, name string, caCert *x509.Certificate, caKey *ecdsa.PrivateKey, serial int64) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	tmpl := &x509.Certificate{SerialNumber: big.NewInt(serial), Subject: pkix.Name{CommonName: base},
		DNSNames: []string{name}, NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour),
		KeyUsage: x509.KeyUsageDigitalSignature, ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, caCert, key.Public(), caKey)
	if err != nil {
		t.Fatal(err)
	}
	keyDER, _ := x509.MarshalPKCS8PrivateKey(key)
	os.WriteFile(filepath.Join(dir, base+".crt"), pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}), 0o600)
	os.WriteFile(filepath.Join(dir, base+".key"), pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: keyDER}), 0o600)
}

func TestRunTLS(t *testing.T) {
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

	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, r.Header.Get("X-Forwarded-Proto"))
	}))
	defer up.Close()
	cfgPath := filepath.Join(dir, "steer.json")
	writeTLSConfig := func(tlsBlock string) {
		js := fmt.Sprintf(`{"listen":"127.0.0.1:0","admin_listen":"127.0.0.1:0","routes":[{"cluster":"web"}],
			"clusters":[{"name":"web","endpoints":[{"address":%q}]}]%s}`, up.Listener.Addr().String(), tlsBlock)
		if err := os.WriteFile(cfgPath, []byte(js), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	// Relative paths resolve against the configuration file's directory.
	writeTLSConfig(`,"tls":{"certificates":[{"cert_file":"site.crt","key_file":"site.key"}],"handshake_timeout":"1s"}`)

	ctx, cancel := context.WithCancel(context.Background())
	hup := make(chan os.Signal, 1)
	addrs := make(chan [2]string, 1)
	done := make(chan error, 1)
	logs := &logBuf{}
	go func() {
		done <- run(ctx, env(map[string]string{"STEER_CONFIG": cfgPath, "STEER_DRAIN_PERIOD": "0s",
			"STEER_SHUTDOWN_TIMEOUT": "2s"}), logs, hup, func(p, a string) { addrs <- [2]string{p, a} })
	}()
	var ad [2]string
	select {
	case ad = <-addrs:
	case err := <-done:
		t.Fatal(err)
	}

	tr := &http.Transport{TLSClientConfig: &tls.Config{RootCAs: pool, ServerName: "steer.test"}}
	defer tr.CloseIdleConnections()
	client := &http.Client{Transport: tr, Timeout: 5 * time.Second}
	fetch := func() (body, cn string, reused bool) {
		t.Helper()
		var info httptrace.GotConnInfo
		rctx := httptrace.WithClientTrace(context.Background(), &httptrace.ClientTrace{GotConn: func(i httptrace.GotConnInfo) { info = i }})
		req, _ := http.NewRequestWithContext(rctx, http.MethodGet, "https://"+ad[0]+"/", nil)
		resp, err := client.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		b, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		return string(b), resp.TLS.PeerCertificates[0].Subject.CommonName, info.Reused
	}
	if body, cn, _ := fetch(); body != "https" || cn != "site" {
		t.Fatalf("first request: proto %q, certificate %s", body, cn)
	}
	// Plain HTTP to the TLS listener fails the handshake and is not proxied.
	if resp, err := http.Get("http://" + ad[0] + "/"); err == nil && resp.StatusCode == http.StatusOK {
		t.Fatal("plain HTTP was served on the TLS listener")
	}

	// Rotate the certificate files and reload with SIGHUP.
	writeCert(t, dir, "rotated", "steer.test", caCert, caKey, 3)
	writeTLSConfig(`,"tls":{"certificates":[{"cert_file":"rotated.crt","key_file":"rotated.key"}]}`)
	hup <- os.Interrupt
	waitLog(t, logs, `"msg":"reloaded on SIGHUP"`)
	if body, cn, reused := fetch(); !reused || cn != "site" || body != "https" {
		t.Fatalf("live connection after reload: reused=%v cert=%s body=%q", reused, cn, body)
	}
	tr.CloseIdleConnections()
	if _, cn, reused := fetch(); reused || cn != "rotated" {
		t.Fatalf("new connection after reload: reused=%v cert=%s, want rotated", reused, cn)
	}

	// Reloads that would turn TLS off or point at a missing key are refused.
	writeTLSConfig(``)
	hup <- os.Interrupt
	waitLog(t, logs, "tls cannot be turned on or off")
	writeTLSConfig(`,"tls":{"certificates":[{"cert_file":"rotated.crt","key_file":"missing.key"}]}`)
	hup <- os.Interrupt
	waitLog(t, logs, "missing.key")
	tr.CloseIdleConnections()
	if _, cn, _ := fetch(); cn != "rotated" {
		t.Fatalf("after rejected reloads the certificate is %s", cn)
	}

	cancel()
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func waitLog(t *testing.T, logs *logBuf, s string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for !strings.Contains(logs.String(), s) {
		if time.Now().After(deadline) {
			t.Fatalf("log never mentioned %q:\n%s", s, logs.String())
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func TestStartupTLSError(t *testing.T) {
	dir := t.TempDir()
	cfgPath := filepath.Join(dir, "steer.json")
	js := `{"listen":"127.0.0.1:0","admin_listen":"127.0.0.1:0","routes":[{"cluster":"web"}],
		"clusters":[{"name":"web","endpoints":[{"address":"127.0.0.1:1"}]}],
		"tls":{"certificates":[{"cert_file":"none.crt","key_file":"none.key"}]}}`
	os.WriteFile(cfgPath, []byte(js), 0o600)
	err := run(context.Background(), env(map[string]string{"STEER_CONFIG": cfgPath}), io.Discard, nil, nil)
	if err == nil || !strings.Contains(err.Error(), "none.crt") {
		t.Fatalf("run with a missing certificate: %v", err)
	}
}
