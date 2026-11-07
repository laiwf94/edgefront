package main

import "math/bits"

// splitMix is SplitMix64 with Lemire's multiply-shift for bounded draws: the
// seeded generator diffdrive hands steer's balancers in place of
// math/rand/v2, so that the Rust port, given the same seed, draws the same
// numbers. edgefront_proxy::balancer::rng::SplitMix64 is its twin; both are
// checked against the same reference outputs.
type splitMix struct{ state uint64 }

func newSplitMix(seed uint64) *splitMix { return &splitMix{state: seed} }

// Uint64 returns the next 64-bit output.
func (s *splitMix) Uint64() uint64 {
	s.state += 0x9e3779b97f4a7c15
	z := s.state
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

// IntN returns a number in [0, n), the high word of the 128-bit product of
// the next output and n. n must be positive.
func (s *splitMix) IntN(n int) int {
	if n <= 0 {
		panic("splitMix.IntN: n must be positive")
	}
	hi, _ := bits.Mul64(s.Uint64(), uint64(n))
	return int(hi)
}
