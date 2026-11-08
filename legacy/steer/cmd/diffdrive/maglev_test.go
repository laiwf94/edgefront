package main

import (
	"encoding/binary"
	"fmt"
	"hash/fnv"
	"os"
	"strings"
	"testing"

	"github.com/laiwf94/steer/internal/balancer"
	"github.com/laiwf94/steer/internal/endpoint"
)

// maglevVectors is shared with the Rust port, which rebuilds every table
// with its own Maglev and must produce the same bytes. Run with
// STEER_BLESS=1 to rewrite it from steer.
const maglevVectors = "../../../../crates/edgefront-proxy/tests/fixtures/steer_maglev.txt"

const (
	maglevSets  = 50
	maglevPicks = 60
)

func TestMaglevVectors(t *testing.T) {
	got := genMaglevVectors()
	if os.Getenv("STEER_BLESS") == "1" {
		if err := os.WriteFile(maglevVectors, []byte(got), 0o644); err != nil {
			t.Fatal(err)
		}
		return
	}
	want, err := os.ReadFile(maglevVectors)
	if err != nil {
		t.Fatal(err)
	}
	if string(want) != got {
		gl, wl := strings.Split(got, "\n"), strings.Split(string(want), "\n")
		for i := range min(len(gl), len(wl)) {
			if gl[i] != wl[i] {
				t.Fatalf("%s:%d: steer now says %.80q, fixture %.80q (STEER_BLESS=1 rewrites it)", maglevVectors, i+1, gl[i], wl[i])
			}
		}
		t.Fatalf("%s: steer now writes %d lines, fixture has %d", maglevVectors, len(gl), len(wl))
	}
}

// The generated sets must stay within what the fixture format can carry.
func TestMaglevVectorsFit(t *testing.T) {
	for _, line := range strings.Split(genMaglevVectors(), "\n") {
		if f := strings.Fields(line); len(f) == 4 && f[0] == "maglev" {
			var n int
			fmt.Sscan(f[3], &n)
			if n > 255 {
				t.Fatalf("%s: more endpoints than one hex byte can name", line)
			}
		}
	}
}

// tableDigest is FNV-1a over the table as steer holds it in memory: one
// little-endian int32 per slot, -1 for an empty slot.
func tableDigest(entries []int32) uint64 {
	h := fnv.New64a()
	var b [4]byte
	for _, e := range entries {
		binary.LittleEndian.PutUint32(b[:], uint32(e))
		h.Write(b[:])
	}
	return h.Sum64()
}

func writeTable(b *strings.Builder, cands []int, all bool, entries []int32) {
	c := list(cands)
	if all {
		c = "*"
	}
	fmt.Fprintf(b, "t %s %016x ", c, tableDigest(entries))
	for _, e := range entries {
		fmt.Fprintf(b, "%02x", uint8(e))
	}
	b.WriteByte('\n')
}

// maglevPrimes are the table sizes the random sets draw from.
var maglevPrimes = []int{7, 13, 101, 211, 251, 503, 1009, 2003}

func maglevAddress(g *splitMix, i int) string {
	switch g.IntN(4) {
	case 0:
		return fmt.Sprintf("[2001:db8::%x]:%d", g.IntN(1<<16), 1+g.IntN(65535))
	case 1:
		return fmt.Sprintf("backend-%d.svc.example:%d", i, 8000+g.IntN(100))
	default:
		return fmt.Sprintf("10.%d.%d.%d:%d", g.IntN(256), g.IntN(256), g.IntN(256), 1+g.IntN(65535))
	}
}

func genMaglevVectors() string {
	var b strings.Builder
	b.WriteString(`# Maglev lookup tables and picks from steer's balancer.Maglev, written by
# TestMaglevVectors in legacy/steer/cmd/diffdrive (STEER_BLESS=1).
#
#   h HEX HASH             HashKey of the bytes HEX (- for none), in hex
#   maglev SEED SIZE N     a Maglev of table size SIZE over N endpoints, whose
#                          keyless picks draw from a SplitMix64 seeded with SEED
#   e ADDRESS WEIGHT       one endpoint, N lines in index order
#   t CANDS DIGEST HEX     Populate(CANDS): DIGEST is FNV-1a 64 over the table
#                          as little-endian int32s, HEX one byte per slot
#                          (ff for an empty slot)
#   p CANDS TRIED KEY -> EP
#                          a pick: CANDS is * (every endpoint) or a list, TRIED
#                          a list or -, KEY the hash key in hex or - for none,
#                          EP the pick or -1
#   end
`)
	fmt.Fprintf(&b, "h - %016x\n", balancer.HashKey(""))
	for _, s := range []string{"a", "alice", "user-1", "X-User-Id", "\x00\xff", strings.Repeat("k", 300)} {
		fmt.Fprintf(&b, "h %x %016x\n", s, balancer.HashKey(s))
	}
	seeds := newSplitMix(65537)
	for set := range maglevSets {
		genMaglev(&b, set, seeds.Uint64())
	}
	return b.String()
}

func genMaglev(b *strings.Builder, set int, seed uint64) {
	g := newSplitMix(seed ^ 0x5eed)
	var n, size int
	switch set {
	case 0, 1:
		// The table the roadmap names: 100 endpoints, the default size.
		n, size = 100, balancer.DefaultMaglevTableSize
	default:
		n = 1 + g.IntN(12)
		if g.IntN(4) == 0 {
			n = 1 + g.IntN(200)
		}
		var sizes []int
		for _, p := range maglevPrimes {
			if p > n {
				sizes = append(sizes, p)
			}
		}
		size = sizes[g.IntN(len(sizes))]
	}
	eps := make([]*endpoint.Endpoint, n)
	for i := range n {
		w := 1
		switch {
		case set == 0:
		case set == 1:
			w = 1 + i%3
		case g.IntN(10) < 3:
			w = 1 + g.IntN(10)
		case g.IntN(20) == 0:
			w = 1 + g.IntN(1000)
		}
		addr := fmt.Sprintf("10.0.%d.%d:8080", i/250, i%250+1)
		if set > 1 {
			addr = maglevAddress(g, i)
		}
		eps[i] = endpoint.New(addr, w, 0)
	}
	m, err := balancer.NewMaglev(eps, size, newSplitMix(seed).IntN)
	if err != nil {
		panic(err)
	}
	fmt.Fprintf(b, "maglev %d %d %d\n", seed, size, n)
	for _, ep := range eps {
		fmt.Fprintf(b, "e %s %d\n", ep.Address, ep.Weight)
	}
	all := make([]int, n)
	for i := range all {
		all[i] = i
	}
	writeTable(b, all, true, m.Populate(all))
	// One endpoint gone, as when it fails its health check, and a random
	// subset, as one priority level. The two full-size sets keep to one
	// table each, to keep the fixture small.
	if set > 1 && n > 1 {
		gone := g.IntN(n)
		var rest []int
		for i := range n {
			if i != gone {
				rest = append(rest, i)
			}
		}
		writeTable(b, rest, false, m.Populate(rest))
	}
	if set > 1 {
		cands, _, _ := pickArgs(g, n)
		if len(cands) > 0 {
			writeTable(b, cands, false, m.Populate(cands))
		}
	}
	for range maglevPicks {
		cands, all, tried := pickArgs(g, n)
		key, hasKey := uint64(0), g.IntN(10) < 8
		k := "-"
		if hasKey {
			key = g.Uint64()
			if g.IntN(4) == 0 {
				key = balancer.HashKey(fmt.Sprintf("user-%d", g.IntN(1000)))
			}
			k = fmt.Sprintf("%x", key)
		}
		got := m.Pick(cands, skipOf(tried), key, hasKey)
		c := list(cands)
		if all {
			c = "*"
		}
		fmt.Fprintf(b, "p %s %s %s -> %d\n", c, list(tried), k, got)
	}
	b.WriteString("end\n")
}
