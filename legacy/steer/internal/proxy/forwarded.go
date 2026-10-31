package proxy

import (
	"net"
	"net/http/httputil"
	"net/netip"
	"strings"
)

// maxForwardedHops bounds how much of a trusted X-Forwarded-For chain is kept.
const maxForwardedHops = 32

// peerAddr parses the connection's remote address.
func peerAddr(remoteAddr string) (netip.Addr, bool) {
	host, _, err := net.SplitHostPort(remoteAddr)
	if err != nil {
		host = remoteAddr
	}
	a, err := netip.ParseAddr(host)
	if err != nil {
		return netip.Addr{}, false
	}
	return a.Unmap().WithZone(""), true
}

func trustedAddr(a netip.Addr, trusted []netip.Prefix) bool {
	for _, p := range trusted {
		if p.Contains(a) {
			return true
		}
	}
	return false
}

// parseForwardedFor splits X-Forwarded-For values into addresses, dropping
// entries that are not IP addresses and keeping at most the last
// maxForwardedHops (the ones nearest to us).
func parseForwardedFor(values []string) []netip.Addr {
	var out []netip.Addr
	for _, v := range values {
		for part := range strings.SplitSeq(v, ",") {
			a, err := netip.ParseAddr(strings.TrimSpace(part))
			if err != nil {
				continue
			}
			out = append(out, a.Unmap().WithZone(""))
		}
	}
	if len(out) > maxForwardedHops {
		out = out[len(out)-maxForwardedHops:]
	}
	return out
}

// ClientIP returns the address of the client as far as it can be trusted:
// the connection's peer, or, when the peer is a trusted proxy, the rightmost
// X-Forwarded-For entry that is not itself a trusted proxy. Entries left of
// the first untrusted hop could have been written by anyone.
func ClientIP(remoteAddr string, xff []string, trusted []netip.Prefix) string {
	peer, ok := peerAddr(remoteAddr)
	if !ok {
		return ""
	}
	if !trustedAddr(peer, trusted) {
		return peer.String()
	}
	chain := parseForwardedFor(xff)
	for i := len(chain) - 1; i >= 0; i-- {
		if !trustedAddr(chain[i], trusted) {
			return chain[i].String()
		}
	}
	if len(chain) > 0 {
		return chain[0].String()
	}
	return peer.String()
}

// setForwarded writes X-Forwarded-For, -Proto and -Host on the outbound
// request. ReverseProxy has already removed any the client sent; the inbound
// values are carried over only when the peer is a trusted proxy, and only in
// well-formed shape.
func setForwarded(pr *httputil.ProxyRequest, trusted []netip.Prefix) {
	in := pr.In
	peer, ok := peerAddr(in.RemoteAddr)
	fromTrusted := ok && trustedAddr(peer, trusted)

	var chain []string
	if fromTrusted {
		for _, a := range parseForwardedFor(in.Header.Values("X-Forwarded-For")) {
			chain = append(chain, a.String())
		}
	}
	if ok {
		chain = append(chain, peer.String())
	}
	if len(chain) > 0 {
		pr.Out.Header.Set("X-Forwarded-For", strings.Join(chain, ", "))
	}

	proto := "http"
	if in.TLS != nil {
		proto = "https"
	}
	if fromTrusted {
		if p := strings.ToLower(in.Header.Get("X-Forwarded-Proto")); p == "http" || p == "https" {
			proto = p
		}
	}
	pr.Out.Header.Set("X-Forwarded-Proto", proto)

	host := in.Host
	if fromTrusted {
		if h := in.Header.Get("X-Forwarded-Host"); h != "" && validHostHeader(h) {
			host = h
		}
	}
	if validHostHeader(host) {
		pr.Out.Header.Set("X-Forwarded-Host", host)
	}
}

// validHostHeader accepts printable ASCII without spaces, up to 255 bytes.
func validHostHeader(h string) bool {
	if len(h) == 0 || len(h) > 255 {
		return false
	}
	for i := 0; i < len(h); i++ {
		if h[i] <= ' ' || h[i] >= 0x7f || h[i] == ',' {
			return false
		}
	}
	return true
}
