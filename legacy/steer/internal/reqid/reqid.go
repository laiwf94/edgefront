// Package reqid accepts or generates request IDs.
package reqid

import (
	"crypto/rand"
	"encoding/hex"
	"net/http"
)

// Header carries the request ID in both directions.
const Header = "X-Request-Id"

// MaxLength is the longest inbound ID accepted.
const MaxLength = 128

// FromRequest returns the inbound X-Request-Id when it is safe to log and
// forward (1 to 128 characters from A-Z a-z 0-9 . _ : -), or a new random one.
func FromRequest(r *http.Request) string {
	if id := r.Header.Get(Header); Valid(id) {
		return id
	}
	return New()
}

// Valid reports whether id is an acceptable inbound request ID.
func Valid(id string) bool {
	if len(id) == 0 || len(id) > MaxLength {
		return false
	}
	for i := 0; i < len(id); i++ {
		ch := id[i]
		if !(ch >= 'a' && ch <= 'z' || ch >= 'A' && ch <= 'Z' || ch >= '0' && ch <= '9' || ch == '.' || ch == '_' || ch == ':' || ch == '-') {
			return false
		}
	}
	return true
}

// New returns 24 random hex characters.
func New() string {
	var b [12]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}
