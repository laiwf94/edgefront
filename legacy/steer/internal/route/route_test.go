package route

import (
	"fmt"
	"math/rand/v2"
	"strings"
	"testing"

	"github.com/laiwf94/steer/internal/config"
)

func TestMatch(t *testing.T) {
	r := New([]config.Route{
		{Host: "*", PathPrefix: "/", Cluster: "default"},
		{Host: "*", PathPrefix: "/api/", Cluster: "api"},
		{Host: "*", PathPrefix: "/api/v2/", Cluster: "api-v2"},
		{Host: "shop.example.com", PathPrefix: "/", Cluster: "shop"},
		{Host: "*.example.com", PathPrefix: "/", Cluster: "wild"},
		{Host: "*.eu.example.com", PathPrefix: "/static/", Cluster: "eu-static"},
	})
	cases := []struct{ host, path, want string }{
		{"other.org", "/", "default"},
		{"other.org", "/api/users", "api"},
		{"other.org", "/api/v2/users", "api-v2"},
		{"other.org", "/api", "default"},
		{"SHOP.example.com:8443", "/api/x", "shop"},
		{"shop.example.com.", "/", "shop"},
		{"a.example.com", "/api/x", "wild"},
		{"a.b.example.com", "/", "wild"},
		{"example.com", "/", "default"},
		{"x.eu.example.com", "/static/a.css", "eu-static"},
		{"x.eu.example.com", "/index.html", ""}, // no fall-through to *.example.com
		{"[::1]:8080", "/api/", "api"},
		{"[::1]", "/", "default"},
	}
	for _, c := range cases {
		got, ok := r.Match(c.host, c.path)
		if (c.want == "") == ok || got.Cluster != c.want {
			t.Errorf("Match(%q, %q) = %q %v, want %q", c.host, c.path, got.Cluster, ok, c.want)
		}
	}
	if _, ok := New([]config.Route{{Host: "a.com", PathPrefix: "/", Cluster: "a"}}).Match("b.com", "/"); ok {
		t.Fatal("matched without a catch-all host")
	}
}

func TestCleanPath(t *testing.T) {
	cases := map[string]string{
		"":                  "/",
		"/":                 "/",
		"a/b":               "/a/b",
		"/a/b/":             "/a/b/",
		"//a///b//":         "/a/b/",
		"/public/../admin":  "/admin",
		"/../../etc/passwd": "/etc/passwd",
		"/a/./b/.":          "/a/b",
		"/a/..":             "/",
		"/a/../":            "/",
	}
	for in, want := range cases {
		if got := CleanPath(in); got != want {
			t.Errorf("CleanPath(%q) = %q, want %q", in, got, want)
		}
	}
}

// reference matches by brute force over every route: classify how the host
// pattern matches, keep the most specific pattern, then the longest prefix.
func reference(routes []config.Route, host, p string) (string, bool) {
	host = NormalizeHost(host)
	rank := func(pattern string) int { // higher is more specific; -1 no match
		switch {
		case pattern == host:
			return 1 << 20
		case strings.HasPrefix(pattern, "*.") && len(host) > len(pattern)-1 && strings.HasSuffix(host, pattern[1:]):
			return len(pattern)
		case pattern == "*":
			return 0
		}
		return -1
	}
	bestRank := -1
	for _, rt := range routes {
		bestRank = max(bestRank, rank(rt.Host))
	}
	if bestRank < 0 {
		return "", false
	}
	best, bestLen := "", -1
	for _, rt := range routes {
		if rank(rt.Host) == bestRank && strings.HasPrefix(p, rt.PathPrefix) && len(rt.PathPrefix) > bestLen {
			best, bestLen = rt.Cluster, len(rt.PathPrefix)
		}
	}
	return best, bestLen >= 0
}

func TestMatchesReference(t *testing.T) {
	rng := rand.New(rand.NewPCG(9, 9))
	hosts := []string{"*", "a.com", "b.a.com", "*.a.com", "*.b.a.com", "c.org", "*.org"}
	segs := []string{"api", "v1", "v2", "static", "x"}
	randPath := func() string {
		var b strings.Builder
		for range rng.IntN(4) {
			b.WriteString("/" + segs[rng.IntN(len(segs))])
		}
		if b.Len() == 0 || rng.IntN(3) == 0 {
			b.WriteString("/")
		}
		return b.String()
	}
	for iter := 0; iter < 2000; iter++ {
		var routes []config.Route
		seen := map[[2]string]bool{}
		for k := 0; k < 1+rng.IntN(12); k++ {
			rt := config.Route{Host: hosts[rng.IntN(len(hosts))], PathPrefix: randPath(), Cluster: fmt.Sprint("c", k)}
			if key := [2]string{rt.Host, rt.PathPrefix}; !seen[key] {
				seen[key] = true
				routes = append(routes, rt)
			}
		}
		r := New(routes)
		for q := 0; q < 50; q++ {
			host := []string{"a.com", "b.a.com", "x.b.a.com", "z.a.com", "c.org", "d.c.org", "e.net", "A.COM:80"}[rng.IntN(8)]
			p := randPath()
			got, ok := r.Match(host, p)
			want, wantOK := reference(routes, host, p)
			if ok != wantOK || got.Cluster != want {
				t.Fatalf("routes %+v\nMatch(%q, %q) = %q %v, reference %q %v", routes, host, p, got.Cluster, ok, want, wantOK)
			}
		}
	}
}

func FuzzCleanPath(f *testing.F) {
	for _, s := range []string{"", "/", "/a/../b", "//x//", "/./.", "a", "/%2e%2e/"} {
		f.Add(s)
	}
	f.Fuzz(func(t *testing.T, s string) {
		c := CleanPath(s)
		if !strings.HasPrefix(c, "/") {
			t.Fatalf("CleanPath(%q) = %q does not start with /", s, c)
		}
		if CleanPath(c) != c {
			t.Fatalf("CleanPath not idempotent: %q -> %q -> %q", s, c, CleanPath(c))
		}
		for _, seg := range strings.Split(c, "/") {
			if seg == ".." || seg == "." {
				t.Fatalf("CleanPath(%q) = %q keeps a dot segment", s, c)
			}
		}
		if strings.Contains(c, "//") {
			t.Fatalf("CleanPath(%q) = %q has an empty segment", s, c)
		}
	})
}

func FuzzMatch(f *testing.F) {
	routes := []config.Route{
		{Host: "*", PathPrefix: "/", Cluster: "default"},
		{Host: "*", PathPrefix: "/api/", Cluster: "api"},
		{Host: "a.com", PathPrefix: "/x", Cluster: "ax"},
		{Host: "*.a.com", PathPrefix: "/", Cluster: "wild"},
	}
	r := New(routes)
	f.Add("a.com", "/x/y")
	f.Add("[::1]:80", "/api/")
	f.Add("B.A.COM.", "/")
	f.Fuzz(func(t *testing.T, host, p string) {
		p = CleanPath(p)
		got, ok := r.Match(host, p)
		want, wantOK := reference(routes, host, p)
		if ok != wantOK || got.Cluster != want {
			t.Fatalf("Match(%q, %q) = %q %v, reference %q %v", host, p, got.Cluster, ok, want, wantOK)
		}
	})
}

func BenchmarkMatch(b *testing.B) {
	var routes []config.Route
	for i := 0; i < 100; i++ {
		routes = append(routes, config.Route{Host: "*", PathPrefix: fmt.Sprintf("/svc%d/", i), Cluster: "c"})
	}
	r := New(routes)
	for b.Loop() {
		r.Match("example.com", "/svc99/users/42")
	}
}
