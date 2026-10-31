package reqid

import (
	"net/http/httptest"
	"strings"
	"testing"
)

func TestFromRequest(t *testing.T) {
	r := httptest.NewRequest("GET", "/", nil)
	r.Header.Set(Header, "abc-123.x_y:z")
	if got := FromRequest(r); got != "abc-123.x_y:z" {
		t.Fatalf("safe id replaced: %q", got)
	}
	for _, bad := range []string{"", "has space", "new\nline", "<script>", strings.Repeat("a", 129), "ü"} {
		r.Header.Set(Header, bad)
		got := FromRequest(r)
		if got == bad || len(got) != 24 || !Valid(got) {
			t.Errorf("unsafe id %q -> %q", bad, got)
		}
	}
	if New() == New() {
		t.Fatal("ids repeat")
	}
}

func FuzzValid(f *testing.F) {
	f.Add("abc")
	f.Add("a b")
	f.Fuzz(func(t *testing.T, s string) {
		if !Valid(s) {
			return
		}
		if len(s) > MaxLength || strings.ContainsAny(s, " \r\n\t\"<>,;") {
			t.Fatalf("accepted %q", s)
		}
	})
}
