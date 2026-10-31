package balancer

import (
	"encoding/binary"
	"fmt"
	"hash/fnv"
	"sync"
	"sync/atomic"

	"github.com/laiwf94/steer/internal/endpoint"
)

// DefaultMaglevTableSize is the lookup table size, a prime as the Maglev
// paper requires (and Envoy's default). It should be at least 100 times the
// number of endpoints for even spreading.
const DefaultMaglevTableSize = 65537

// maglevCacheSize bounds the number of lookup tables kept, one per distinct
// set of available endpoints (typically one per priority level).
const maglevCacheSize = 8

// Maglev is consistent hashing with Google's Maglev lookup table (Eisenbud
// et al., NSDI). Every endpoint has a pseudo-random permutation of table
// slots derived from its address; endpoints take turns claiming their next
// free slot until the table is full. A key maps to table[hash % size].
//
// Removing one endpoint of N reassigns its slots and only a small fraction
// of the others', so most keys keep their endpoint. Weights are honoured by
// giving an endpoint a turn in proportion to its weight relative to the
// heaviest. Tables depend only on endpoint addresses and weights, so every
// steer instance with the same configuration maps keys identically.
type Maglev struct {
	eps  []*endpoint.Endpoint
	size int
	rand func(int) int
	// offset and skip define each endpoint's permutation:
	// slot(j) = (offset + j*skip) mod size.
	offset, skip []uint64

	last  atomic.Pointer[maglevTable]
	mu    sync.Mutex
	cache map[string]*maglevTable
	order []string // insertion order for eviction
}

type maglevTable struct {
	key     string
	entries []int32 // endpoint index per slot
}

// NewMaglev builds a Maglev policy. size must be a prime larger than len(eps).
func NewMaglev(eps []*endpoint.Endpoint, size int, rand func(int) int) (*Maglev, error) {
	if size <= len(eps) || !isPrime(size) {
		return nil, fmt.Errorf("balancer: maglev table size %d must be a prime larger than the endpoint count", size)
	}
	m := &Maglev{eps: eps, size: size, rand: rand, cache: map[string]*maglevTable{},
		offset: make([]uint64, len(eps)), skip: make([]uint64, len(eps))}
	for i, ep := range eps {
		m.offset[i] = hashString("maglev-offset\x00"+ep.Address) % uint64(size)
		m.skip[i] = hashString("maglev-skip\x00"+ep.Address)%uint64(size-1) + 1
	}
	return m, nil
}

// HashKey hashes a request key (such as a header value) for Pick.
func HashKey(s string) uint64 { return hashString(s) }

// hashString is FNV-1a followed by the SplitMix64 finalizer, so short,
// similar strings spread over the whole 64-bit range.
func hashString(s string) uint64 {
	h := fnv.New64a()
	h.Write([]byte(s))
	x := h.Sum64()
	x ^= x >> 30
	x *= 0xbf58476d1ce4e5b9
	x ^= x >> 27
	x *= 0x94d049bb133111eb
	x ^= x >> 31
	return x
}

func isPrime(n int) bool {
	if n < 2 {
		return false
	}
	for d := 2; d*d <= n; d++ {
		if n%d == 0 {
			return false
		}
	}
	return true
}

// Pick implements Policy. Without a key it picks a random candidate. A key
// whose slot belongs to a skipped endpoint walks forward through the table to
// the next slot owned by an unskipped one, so retries are deterministic too.
func (m *Maglev) Pick(cands []int, skip func(int) bool, key uint64, hasKey bool) int {
	if len(cands) == 0 {
		return -1
	}
	if !hasKey {
		start := m.rand(len(cands))
		for k := range cands {
			if i := cands[(start+k)%len(cands)]; !skip(i) {
				return i
			}
		}
		return -1
	}
	t := m.table(cands)
	slot := int(key % uint64(m.size))
	for range m.size {
		if i := int(t.entries[slot]); !skip(i) {
			return i
		}
		slot++
		if slot == m.size {
			slot = 0
		}
	}
	return -1
}

func candidateKey(cands []int) string {
	b := make([]byte, 2*len(cands))
	for k, i := range cands {
		binary.LittleEndian.PutUint16(b[2*k:], uint16(i))
	}
	return string(b)
}

func (m *Maglev) table(cands []int) *maglevTable {
	key := candidateKey(cands)
	if t := m.last.Load(); t != nil && t.key == key {
		return t
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	t, ok := m.cache[key]
	if !ok {
		t = &maglevTable{key: key, entries: m.Populate(cands)}
		if len(m.order) == maglevCacheSize {
			delete(m.cache, m.order[0])
			m.order = append(m.order[:0], m.order[1:]...)
		}
		m.cache[key] = t
		m.order = append(m.order, key)
	}
	m.last.Store(t)
	return t
}

// Populate builds the lookup table for the given endpoint indices: the
// paper's Algorithm 1, extended with weights. In round r an endpoint with
// weight w takes a turn when floor(r*w/maxW) grows, so over the whole table it
// claims slots in proportion to w.
func (m *Maglev) Populate(cands []int) []int32 {
	entries := make([]int32, m.size)
	for i := range entries {
		entries[i] = -1
	}
	if len(cands) == 0 {
		return entries
	}
	maxW := 0
	for _, i := range cands {
		maxW = max(maxW, m.eps[i].Weight)
	}
	next := make([]uint64, len(cands))
	filled := 0
	for round := 1; ; round++ {
		for k, i := range cands {
			w := m.eps[i].Weight
			if round*w/maxW == (round-1)*w/maxW {
				continue
			}
			size := uint64(m.size)
			c := (m.offset[i] + next[k]*m.skip[i]) % size
			for entries[c] >= 0 {
				next[k]++
				c = (m.offset[i] + next[k]*m.skip[i]) % size
			}
			entries[c] = int32(i)
			next[k]++
			filled++
			if filled == m.size {
				return entries
			}
		}
	}
}
