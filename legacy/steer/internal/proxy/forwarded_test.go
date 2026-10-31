package proxy

import (
	"fmt"
	"net/netip"
	"strings"
	"testing"
)

func prefixes(ss ...string) []netip.Prefix {
	var out []netip.Prefix
	for _, s := range ss {
		out = append(out, netip.MustParsePrefix(s))
	}
	return out
}

func TestClientIP(t *testing.T) {
	trusted := prefixes("10.0.0.0/8", "::1/128")
	cases := []struct {
		remote string
		xff    []string
		want   string
	}{
		{"203.0.113.5:4000", []string{"1.1.1.1"}, "203.0.113.5"},         // untrusted peer: XFF ignored
		{"10.0.0.2:4000", []string{"1.1.1.1, 10.0.0.9"}, "1.1.1.1"},      // skip trusted hops
		{"10.0.0.2:4000", []string{"6.6.6.6, 2.2.2.2"}, "2.2.2.2"},       // rightmost untrusted wins
		{"10.0.0.2:4000", []string{"10.0.0.3"}, "10.0.0.3"},              // all trusted: leftmost
		{"10.0.0.2:4000", nil, "10.0.0.2"},                               // no header
		{"10.0.0.2:4000", []string{"garbage, 3.3.3.3, nope"}, "3.3.3.3"}, // junk dropped
		{"[::1]:4000", []string{"2001:db8::1"}, "2001:db8::1"},           // IPv6
		{"[::ffff:10.0.0.2]:1", []string{"4.4.4.4"}, "4.4.4.4"},          // v4-mapped peer
		{"not-an-address", []string{"4.4.4.4"}, ""},                      // unparseable peer
		{"10.0.0.2", []string{"5.5.5.5"}, "5.5.5.5"},                     // no port
		{"10.0.0.2:1", []string{"fe80::1%eth0"}, "fe80::1"},              // zone stripped
	}
	for _, c := range cases {
		if got := ClientIP(c.remote, c.xff, trusted); got != c.want {
			t.Errorf("ClientIP(%q, %q) = %q, want %q", c.remote, c.xff, got, c.want)
		}
	}
}

func TestForwardedChainIsBounded(t *testing.T) {
	var parts []string
	for i := 0; i < 100; i++ {
		parts = append(parts, fmt.Sprintf("1.1.1.%d", i))
	}
	got := parseForwardedFor([]string{strings.Join(parts, ",")})
	if len(got) != maxForwardedHops || got[len(got)-1].String() != "1.1.1.99" {
		t.Fatalf("kept %d, last %v", len(got), got[len(got)-1])
	}
}

func TestValidHostHeader(t *testing.T) {
	for _, ok := range []string{"a.com", "a.com:8080", "[::1]:80"} {
		if !validHostHeader(ok) {
			t.Errorf("%q rejected", ok)
		}
	}
	for _, bad := range []string{"", "a b", "a\r\nb", "a,b", strings.Repeat("a", 256), "é.com"} {
		if validHostHeader(bad) {
			t.Errorf("%q accepted", bad)
		}
	}
}

// The client IP is always either the peer or an address from the header,
// and an untrusted peer can never inject one.
func FuzzClientIP(f *testing.F) {
	f.Add("10.0.0.1:80", "1.2.3.4, 10.0.0.2")
	f.Add("8.8.8.8:1", "1.1.1.1")
	f.Add("[::1]:9", "::1,,  ,x")
	trusted := prefixes("10.0.0.0/8", "::1/128")
	f.Fuzz(func(t *testing.T, remote, xff string) {
		got := ClientIP(remote, []string{xff}, trusted)
		peer, ok := peerAddr(remote)
		if !ok {
			if got != "" {
				t.Fatalf("unparseable peer %q gave %q", remote, got)
			}
			return
		}
		if !trustedAddr(peer, trusted) {
			if got != peer.String() {
				t.Fatalf("untrusted peer %q: got %q", remote, got)
			}
			return
		}
		if got == peer.String() {
			return
		}
		found := false
		for _, a := range parseForwardedFor([]string{xff}) {
			found = found || a.String() == got
		}
		if !found {
			t.Fatalf("ClientIP(%q, %q) = %q is neither the peer nor in the header", remote, xff, got)
		}
	})
}
