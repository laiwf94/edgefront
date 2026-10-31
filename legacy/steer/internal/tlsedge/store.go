// Package tlsedge terminates TLS on steer's listeners.
//
// A Store holds the current certificate set and protocol policy as an
// immutable snapshot behind an atomic pointer. Every handshake reads the
// snapshot once through tls.Config.GetConfigForClient, so a reload changes
// what new handshakes see and leaves established connections alone: they
// already finished their handshake and keep serving with the certificate and
// keys they negotiated.
//
// Certificates are chosen by SNI. An exact name beats a wildcard, and a
// wildcard "*.example.com" covers exactly one label ("a.example.com", not
// "a.b.example.com" or "example.com"), as RFC 6125 requires of clients. When
// several certificates carry the same name (an ECDSA and an RSA one, say),
// the first the client can use wins. A handshake that matches nothing gets the
// first configured certificate, or is refused under strict SNI.
package tlsedge

import (
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"strings"
	"sync/atomic"
	"time"

	"github.com/laiwf94/steer/internal/config"
)

// ErrUnknownServerName is returned to the handshake when strict SNI is on and
// the client's server name matches no certificate.
var ErrUnknownServerName = errors.New("tlsedge: no certificate for the requested server name")

// Snapshot is one loaded TLS configuration. It is immutable.
type Snapshot struct {
	cfg      *tls.Config // per-snapshot config handed to handshakes
	exact    map[string][]*tls.Certificate
	wildcard map[string][]*tls.Certificate // keyed by the suffix after "*."
	fallback *tls.Certificate
	strict   bool
	certs    []CertInfo
	leaves   []*x509.Certificate // parallel to certs

	HandshakeTimeout        time.Duration
	MaxConcurrentHandshakes int
}

// CertInfo describes one loaded certificate.
type CertInfo struct {
	CertFile  string
	Names     []string // the names it is selected for
	NotBefore time.Time
	NotAfter  time.Time
}

// Certificates describes the loaded certificates in configuration order.
func (s *Snapshot) Certificates() []CertInfo { return s.certs }

// Load reads every certificate and key in c and builds a snapshot. It fails
// on the first file that cannot be read or parsed, or whose key does not
// match its certificate.
func Load(c *config.TLS) (*Snapshot, error) {
	if c == nil {
		return nil, errors.New("tlsedge: no TLS configuration")
	}
	if len(c.Certificates) == 0 {
		return nil, errors.New("tlsedge: at least one certificate is required")
	}
	s := &Snapshot{
		exact:                   map[string][]*tls.Certificate{},
		wildcard:                map[string][]*tls.Certificate{},
		strict:                  c.StrictSNI,
		HandshakeTimeout:        c.HandshakeTimeout,
		MaxConcurrentHandshakes: c.MaxConcurrentHandshakes,
	}
	if s.HandshakeTimeout <= 0 {
		s.HandshakeTimeout = config.DefaultHandshakeTimeout
	}
	if s.MaxConcurrentHandshakes <= 0 {
		s.MaxConcurrentHandshakes = config.DefaultMaxConcurrentHandshakes
	}
	for i, cc := range c.Certificates {
		cert, err := tls.LoadX509KeyPair(cc.CertFile, cc.KeyFile)
		if err != nil {
			return nil, fmt.Errorf("tls.certificates[%d]: %w", i, err)
		}
		leaf := cert.Leaf // parsed by LoadX509KeyPair
		names := cc.ServerNames
		if names == nil {
			for _, n := range leaf.DNSNames {
				names = append(names, normalize(n))
			}
		}
		if len(names) == 0 && i > 0 {
			return nil, fmt.Errorf("tls.certificates[%d]: %s has no DNS names; set server_names", i, cc.CertFile)
		}
		p := &cert
		if s.fallback == nil {
			s.fallback = p
		}
		for _, n := range dedupe(names) {
			if suffix, ok := strings.CutPrefix(n, "*."); ok {
				s.wildcard[suffix] = append(s.wildcard[suffix], p)
			} else {
				s.exact[n] = append(s.exact[n], p)
			}
		}
		s.certs = append(s.certs, CertInfo{CertFile: cc.CertFile, Names: dedupe(names),
			NotBefore: leaf.NotBefore, NotAfter: leaf.NotAfter})
		s.leaves = append(s.leaves, leaf)
	}
	s.cfg = &tls.Config{
		MinVersion:     c.MinVersion,
		CipherSuites:   c.CipherSuites,
		NextProtos:     []string{"http/1.1"},
		GetCertificate: s.certificate,
	}
	if s.cfg.MinVersion == 0 {
		s.cfg.MinVersion = tls.VersionTLS12
	}
	return s, nil
}

// Warnings lists problems that do not stop the snapshot from serving:
// certificates outside their validity period at now, and configured names a
// certificate does not cover.
func (s *Snapshot) Warnings(now time.Time) []string {
	var out []string
	for _, ci := range s.certs {
		switch {
		case now.After(ci.NotAfter):
			out = append(out, fmt.Sprintf("%s expired at %s", ci.CertFile, ci.NotAfter.UTC().Format(time.RFC3339)))
		case now.Before(ci.NotBefore):
			out = append(out, fmt.Sprintf("%s is not valid until %s", ci.CertFile, ci.NotBefore.UTC().Format(time.RFC3339)))
		}
	}
	for i, ci := range s.certs {
		leaf := s.leaves[i]
		for _, n := range ci.Names {
			host := n
			if suffix, ok := strings.CutPrefix(n, "*."); ok {
				host = "steer-wildcard-probe." + suffix
			}
			if leaf.VerifyHostname(host) != nil {
				out = append(out, fmt.Sprintf("%s does not cover server name %q", ci.CertFile, n))
			}
		}
	}
	return out
}

// Select returns the certificate a handshake for serverName would get. hello
// may be nil; when set, certificates the client cannot use are skipped.
func (s *Snapshot) Select(serverName string, hello *tls.ClientHelloInfo) (*tls.Certificate, error) {
	name := normalize(serverName)
	if name != "" {
		if c := pick(s.exact[name], hello); c != nil {
			return c, nil
		}
		if label, suffix, ok := strings.Cut(name, "."); ok && label != "" && suffix != "" {
			if c := pick(s.wildcard[suffix], hello); c != nil {
				return c, nil
			}
		}
	}
	if s.strict {
		return nil, ErrUnknownServerName
	}
	return s.fallback, nil
}

func (s *Snapshot) certificate(hello *tls.ClientHelloInfo) (*tls.Certificate, error) {
	return s.Select(hello.ServerName, hello)
}

// pick returns the first certificate the client supports, or the first one
// if it supports none of them (the handshake will then fail on its own
// terms, with a clearer alert than a missing certificate).
func pick(certs []*tls.Certificate, hello *tls.ClientHelloInfo) *tls.Certificate {
	if len(certs) == 0 {
		return nil
	}
	if hello != nil && len(certs) > 1 {
		for _, c := range certs {
			if hello.SupportsCertificate(c) == nil {
				return c
			}
		}
	}
	return certs[0]
}

// normalize lower-cases a server name and drops one trailing dot.
func normalize(n string) string {
	return strings.TrimSuffix(strings.ToLower(n), ".")
}

func dedupe(names []string) []string {
	seen := make(map[string]bool, len(names))
	out := names[:0:0]
	for _, n := range names {
		if !seen[n] {
			seen[n] = true
			out = append(out, n)
		}
	}
	return out
}

// Store publishes the current snapshot to handshakes.
type Store struct {
	cur  atomic.Pointer[Snapshot]
	base *tls.Config
}

// NewStore returns a store serving snap.
func NewStore(snap *Snapshot) *Store {
	st := &Store{}
	st.cur.Store(snap)
	st.base = &tls.Config{
		MinVersion: tls.VersionTLS12,
		NextProtos: []string{"http/1.1"},
		GetConfigForClient: func(*tls.ClientHelloInfo) (*tls.Config, error) {
			return st.cur.Load().cfg, nil
		},
	}
	return st
}

// Swap makes snap serve every handshake that starts from now on.
func (st *Store) Swap(snap *Snapshot) { st.cur.Store(snap) }

// Current returns the snapshot new handshakes use.
func (st *Store) Current() *Snapshot { return st.cur.Load() }

// Config returns the server configuration to give tls.Server. It resolves the
// current snapshot per handshake.
func (st *Store) Config() *tls.Config { return st.base }
