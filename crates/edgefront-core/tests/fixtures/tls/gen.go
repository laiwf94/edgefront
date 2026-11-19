//go:build ignore

// Writes the TLS test certificates in this directory: the fixture set of
// steer's internal/tlsedge tests (tlsedge_test.go's newFixture, plus the
// expired, not-yet-valid and nameless certificates its other tests issue),
// made with the same Go code, but with fixed dates so the files can be
// committed. Run it from this directory with `go run gen.go`; it rewrites
// every file, with new keys. `go run gen.go -extra` keeps ca.crt and
// ca.key and rewrites only the certificates added for stapling and client
// authentication (week 5): ocsp (a chain with an OCSP URL),
// ocsp-responder (a delegated OCSP signer), and the client certificates
// (client, client-rotated, client-expired, and client-other from the
// second CA).
package main

import (
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"flag"
	"math/big"
	"net/url"
	"os"
	"time"
)

var (
	start = time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	end   = time.Date(2126, 1, 1, 0, 0, 0, 0, time.UTC)
)

type ca struct {
	cert *x509.Certificate
	key  crypto.Signer
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func write(path string, typ string, der []byte) {
	check(os.WriteFile(path, pem.EncodeToMemory(&pem.Block{Type: typ, Bytes: der}), 0o600))
}

func newCA(base, cn string) *ca {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	check(err)
	tmpl := &x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: cn},
		NotBefore:             start,
		NotAfter:              end,
		IsCA:                  true,
		BasicConstraintsValid: true,
		KeyUsage:              x509.KeyUsageCertSign,
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, key.Public(), key)
	check(err)
	cert, err := x509.ParseCertificate(der)
	check(err)
	write(base+".crt", "CERTIFICATE", der)
	keyDER, err := x509.MarshalPKCS8PrivateKey(key)
	check(err)
	write(base+".key", "PRIVATE KEY", keyDER)
	return &ca{cert: cert, key: key}
}

var serial int64 = 1

type leaf struct {
	base      string
	names     []string
	rsa       bool
	notBefore time.Time
	notAfter  time.Time
	ocsp      []string
	usage     []x509.ExtKeyUsage
	uris      []string
	chain     bool // append the CA to the certificate file
	parent    *ca  // the issuer, when not the main CA
}

func loadCA(base string) *ca {
	pair, err := tls.LoadX509KeyPair(base+".crt", base+".key")
	check(err)
	return &ca{cert: pair.Leaf, key: pair.PrivateKey.(crypto.Signer)}
}

func (c *ca) issue(l leaf) {
	var key crypto.Signer
	var err error
	if l.rsa {
		key, err = rsa.GenerateKey(rand.Reader, 2048)
	} else {
		key, err = ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	}
	check(err)
	if l.notBefore.IsZero() {
		l.notBefore = start
	}
	if l.notAfter.IsZero() {
		l.notAfter = end
	}
	serial++
	if l.usage == nil {
		l.usage = []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}
	}
	var uris []*url.URL
	for _, u := range l.uris {
		p, err := url.Parse(u)
		check(err)
		uris = append(uris, p)
	}
	tmpl := &x509.Certificate{
		SerialNumber: big.NewInt(serial),
		Subject:      pkix.Name{CommonName: l.base},
		DNSNames:     l.names,
		URIs:         uris,
		NotBefore:    l.notBefore,
		NotAfter:     l.notAfter,
		KeyUsage:     x509.KeyUsageDigitalSignature | x509.KeyUsageKeyEncipherment,
		ExtKeyUsage:  l.usage,
		OCSPServer:   l.ocsp,
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, c.cert, key.Public(), c.key)
	check(err)
	keyDER, err := x509.MarshalPKCS8PrivateKey(key)
	check(err)
	out := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	if l.chain {
		out = append(out, pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: c.cert.Raw})...)
	}
	check(os.WriteFile(l.base+".crt", out, 0o600))
	write(l.base+".key", "PRIVATE KEY", keyDER)
}

// extra writes the week 5 certificates, issued by the CA already on disk.
func extra(c *ca) {
	// Serial numbers well clear of the main set's.
	serial = 1000
	for _, l := range []leaf{
		{base: "ocsp", names: []string{"ocsp.example.com"}, ocsp: []string{"http://ocsp.test/"}, chain: true},
		{base: "ocsp-responder", usage: []x509.ExtKeyUsage{x509.ExtKeyUsageOCSPSigning}},
		{base: "client", usage: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
			names: []string{"client.example.com"}, uris: []string{"spiffe://edge.test/ns/web/sa/frontend"}},
		{base: "client-rotated", usage: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
			uris: []string{"spiffe://edge.test/ns/web/sa/rotated"}},
		{base: "client-expired", usage: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
			uris:      []string{"spiffe://edge.test/ns/web/sa/old"},
			notBefore: time.Date(2020, 1, 1, 0, 0, 0, 0, time.UTC), notAfter: time.Date(2020, 1, 3, 0, 0, 0, 0, time.UTC)},
	} {
		c.issue(l)
	}
	other := loadCA("other-ca")
	other.issue(leaf{base: "client-other", usage: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
		uris: []string{"spiffe://elsewhere.test/ns/x/sa/y"}})
}

func main() {
	only := flag.Bool("extra", false, "write only the week 5 certificates")
	flag.Parse()
	if *only {
		extra(loadCA("ca"))
		return
	}
	c := newCA("ca", "steer test CA")
	for _, l := range []leaf{
		{base: "default", names: []string{"default.test"}},
		{base: "exact", names: []string{"api.example.com"}},
		{base: "wild", names: []string{"*.example.com", "example.com"}},
		{base: "dual-ecdsa", names: []string{"dual.test"}},
		{base: "dual-rsa", names: []string{"dual.test"}, rsa: true},
		{base: "rotated", names: []string{"api.example.com"}},
		{base: "nonames"},
		{base: "expired", names: []string{"old.test"},
			notBefore: time.Date(2020, 1, 1, 0, 0, 0, 0, time.UTC), notAfter: time.Date(2020, 1, 3, 0, 0, 0, 0, time.UTC)},
		{base: "future", names: []string{"new.test"},
			notBefore: time.Date(2120, 1, 1, 0, 0, 0, 0, time.UTC), notAfter: time.Date(2120, 1, 3, 0, 0, 0, 0, time.UTC)},
	} {
		c.issue(l)
	}
	// A second CA, for clients that must not trust the first.
	other := newCA("other-ca", "steer other CA")
	other.issue(leaf{base: "other", names: []string{"api.example.com"}})
	check(os.WriteFile("garbage.pem", []byte("not a certificate"), 0o600))
	extra(c)
}
