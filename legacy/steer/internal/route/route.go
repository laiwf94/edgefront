// Package route maps a request's host and path to a cluster.
//
// Matching follows Envoy's virtual hosts: the host picks one virtual host
// first (an exact name beats the longest "*.suffix" wildcard, which beats
// "*"), then the longest path prefix inside that virtual host wins. There is
// no fall-through to a less specific virtual host, so adding a route for one
// host can never change where another host's requests go.
package route

import (
	"net"
	"path"
	"sort"
	"strings"

	"github.com/laiwf94/steer/internal/config"
)

// Router is immutable after New and safe for concurrent use.
type Router struct {
	exact    map[string]*vhost
	suffixes []*vhost // "*.suffix" hosts, longest suffix first
	any      *vhost
}

type vhost struct {
	pattern string
	suffix  string // ".example.com" for "*.example.com"
	routes  []config.Route
}

// New builds a router. Routes must already be validated.
func New(routes []config.Route) *Router {
	r := &Router{exact: map[string]*vhost{}}
	byPattern := map[string]*vhost{}
	for _, rt := range routes {
		v, ok := byPattern[rt.Host]
		if !ok {
			v = &vhost{pattern: rt.Host}
			byPattern[rt.Host] = v
			switch {
			case rt.Host == "*":
				r.any = v
			case strings.HasPrefix(rt.Host, "*."):
				v.suffix = rt.Host[1:]
				r.suffixes = append(r.suffixes, v)
			default:
				r.exact[rt.Host] = v
			}
		}
		v.routes = append(v.routes, rt)
	}
	for _, v := range byPattern {
		sort.SliceStable(v.routes, func(i, j int) bool { return len(v.routes[i].PathPrefix) > len(v.routes[j].PathPrefix) })
	}
	sort.SliceStable(r.suffixes, func(i, j int) bool { return len(r.suffixes[i].suffix) > len(r.suffixes[j].suffix) })
	return r
}

// Match returns the route for a request. host is the Host header as sent
// (any port is ignored); p must be a cleaned path (see CleanPath).
func (r *Router) Match(host, p string) (config.Route, bool) {
	v := r.vhost(NormalizeHost(host))
	if v == nil {
		return config.Route{}, false
	}
	for _, rt := range v.routes {
		if strings.HasPrefix(p, rt.PathPrefix) {
			return rt, true
		}
	}
	return config.Route{}, false
}

func (r *Router) vhost(host string) *vhost {
	if v, ok := r.exact[host]; ok {
		return v
	}
	for _, v := range r.suffixes {
		if len(host) > len(v.suffix) && strings.HasSuffix(host, v.suffix) {
			return v
		}
	}
	return r.any
}

// NormalizeHost lower-cases a Host header value and strips any port and
// trailing dot.
func NormalizeHost(h string) string {
	if hp, _, err := net.SplitHostPort(h); err == nil {
		h = hp
	} else if strings.HasPrefix(h, "[") && strings.HasSuffix(h, "]") {
		h = h[1 : len(h)-1]
	}
	return strings.TrimSuffix(strings.ToLower(h), ".")
}

// CleanPath resolves "." and ".." segments and collapses repeated slashes,
// keeping a trailing slash. Routing on, and forwarding, the cleaned path means
// "/public/../admin" can never match the "/public" route while the upstream
// serves "/admin".
func CleanPath(p string) string {
	if p == "" || p[0] != '/' {
		p = "/" + p
	}
	c := path.Clean(p)
	if strings.HasSuffix(p, "/") && c != "/" {
		c += "/"
	}
	return c
}
