package main

import (
	"encoding/json"
	"slices"
	"strings"
	"testing"
)

func picksQuery(t *testing.T, policy string, panicThreshold int, tableSize int, eps []endpointSpec, script string) string {
	t.Helper()
	q, err := json.Marshal(query{Op: "picks", Policy: policy, PanicThreshold: panicThreshold, Seed: 7,
		TableSize: tableSize, Endpoints: eps, Script: script})
	if err != nil {
		t.Fatal(err)
	}
	return string(q)
}

var twoLevels = []endpointSpec{{"10.0.0.1:80", 1, 0}, {"10.0.0.2:80", 1, 1}}

// The worked example that edgefront-difftest's balancer tests expect the
// Rust balancer to answer in the same words.
func TestPicksWorkedExample(t *testing.T) {
	as := ask(t, picksQuery(t, "round_robin", 50, 0, twoLevels,
		"r - 3\nr 5 1\nh 0 0\ns\nr - 1\ne 1 1000 5000\nr - 1\nt 2000\nr - 2\na 0\nx 0\no 1 5\nc 1\ns"))
	want := []string{
		"0,1,-1",                                // P0 first, then the fallback to P1, then nothing
		"0",                                     // a key means nothing to round robin
		"ok",                                    // P0 loses its only host
		"state h=0,1 t=1,1 l=0,100 panic=false", // so P1 takes everything
		"1",
		"ok", // P1 ejected too: no health anywhere, below the panic threshold
		"0",  // so panic picks from every endpoint
		"ok", // the ejection ends
		"1,-1",
		"ok", "ok", "ok", "ok",
		"state h=0,1 t=1,1 l=0,100 panic=false",
	}
	if len(as) != 1 || !as[0].OK || !slices.Equal(as[0].Lines, want) {
		t.Fatalf("got %+v\nwant %q", as, want)
	}
}

func TestPicksHashAndLeastLoaded(t *testing.T) {
	eps := []endpointSpec{{"10.0.0.1:80", 1, 0}, {"10.0.0.2:80", 1, 0}, {"10.0.0.3:80", 1, 0}}
	// A key stays on its endpoint; a retry walks to another one.
	as := ask(t, picksQuery(t, "maglev", 50, 101, eps, "r 42 1\nr 42 1\nr 42 3\nr - 1"))
	l := as[0].Lines
	if !as[0].OK || l[0] != l[1] || !strings.HasPrefix(l[2], l[0]+",") || strings.Count(l[2], ",") != 2 {
		t.Fatalf("maglev: %+v", as)
	}
	// With endpoint 0 slow and endpoint 1 busy, power of two choices keeps
	// to endpoint 2 whenever it is drawn.
	as = ask(t, picksQuery(t, "least_loaded", 0, 0, eps,
		"o 0 900000000\na 1\na 1\n"+strings.Repeat("r - 1\n", 50)+"s"))
	if !as[0].OK {
		t.Fatalf("least_loaded: %+v", as)
	}
	counts := map[string]int{}
	for _, p := range as[0].Lines[3:53] {
		counts[p]++
	}
	if counts["0"] != 0 || counts["2"] == 0 {
		t.Fatalf("least_loaded picks: %v", counts)
	}
}

func TestPicksBalancerErrors(t *testing.T) {
	as := ask(t,
		picksQuery(t, "round_robin", 0, 0, []endpointSpec{{"h:1", 0, 0}}, "s"),
		picksQuery(t, "maglev", 0, 4, twoLevels, "s"),
		picksQuery(t, "random", 0, 0, twoLevels, "s"),
	)
	want := []string{
		"balancer: endpoint h:1 has weight 0 priority 0",
		"balancer: maglev table size 4 must be a prime larger than the endpoint count",
		`balancer: unknown policy "random"`,
	}
	for i, w := range want {
		if as[i].OK || as[i].Class != "balancer" || as[i].Error != w || as[i].Lines != nil {
			t.Errorf("%d: %+v", i, as[i])
		}
	}
}

func TestPicksMalformedOpsStop(t *testing.T) {
	for _, script := range []string{
		"", "q", "r", "r - 0", "r x 1", "r +5 1", "r 18446744073709551616 1", "s 1",
		"h 2 1", "h 0 2", "h -1 1", "e 0 0 5", "e 0 5 -1", "e 0 5", "c", "a 2", "x 9",
		"o 0 -3", "o 0 9223372036854775808", "t -1", "t", "s\n",
	} {
		_, err := handle(&query{Op: "picks", Endpoints: twoLevels, Script: script})
		if err == nil || !strings.HasPrefix(err.Error(), "op ") {
			t.Errorf("%q: err = %v", script, err)
		}
	}
	// Whitespace between fields is free.
	a, err := handle(&query{Op: "picks", Endpoints: twoLevels, Script: " t\t5 "})
	if err != nil || !slices.Equal(a.Lines, []string{"ok"}) {
		t.Errorf("%+v %v", a, err)
	}
}
