package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"strings"
	"testing"
)

const doc = `{"listen":":8080","routes":[{"host":"*.Example.com","path_prefix":"/api/","cluster":"api"},{"cluster":"web"}],
"clusters":[{"name":"api","endpoints":[{"address":"h:1"}]},{"name":"web","endpoints":[{"address":"h:2"}]}]}`

func hx(s string) string { return hex.EncodeToString([]byte(s)) }

func ask(t *testing.T, lines ...string) []answer {
	t.Helper()
	var out bytes.Buffer
	if err := serve(strings.NewReader(strings.Join(lines, "\n")), &out); err != nil {
		t.Fatal(err)
	}
	var as []answer
	dec := json.NewDecoder(&out)
	for dec.More() {
		var a answer
		if err := dec.Decode(&a); err != nil {
			t.Fatal(err)
		}
		as = append(as, a)
	}
	return as
}

func TestValidate(t *testing.T) {
	as := ask(t,
		`{"op":"validate","config":"`+hx(doc)+`"}`,
		`{"op":"validate","config":"`+hx("{")+`"}`,
		`{"op":"validate","config":"`+hx("{}")+`"}`,
		`{"op":"validate","config":"`+hx(strings.Repeat(" ", 1<<20+1))+`"}`,
	)
	if len(as) != 4 {
		t.Fatalf("got %d answers", len(as))
	}
	if !as[0].OK || as[0].Class != "" || as[0].Results != nil {
		t.Errorf("valid: %+v", as[0])
	}
	for i, class := range []string{"json", "invalid", "size"} {
		a := as[i+1]
		if a.OK || a.Class != class || a.Error == "" {
			t.Errorf("%s: %+v", class, a)
		}
	}
	if !strings.HasPrefix(as[2].Error, "invalid configuration:\nlisten: is required") {
		t.Errorf("error text: %q", as[2].Error)
	}
}

func TestRoute(t *testing.T) {
	reqs := []request{
		{Host: "A.example.com:80", Target: []byte("/x/../api/v1?q")},
		{Host: "example.com", Target: []byte("/api/")},
		{Host: "a.example.com", Target: []byte("/%zz")},
		{Host: "a.example.com", Target: []byte("/apx")},
		{Host: "other", Target: []byte("*")},
	}
	q, _ := json.Marshal(query{Op: "route", Config: hexBytes(doc), Requests: reqs})
	as := ask(t, string(q))
	if len(as) != 1 || !as[0].OK {
		t.Fatalf("%+v", as)
	}
	want := []result{
		{Status: "route", Path: hexBytes("/api/v1"), Host: "*.example.com", PathPrefix: "/api/", Cluster: "api"},
		{Status: "route", Path: hexBytes("/api/"), Host: "*", PathPrefix: "/", Cluster: "web"},
		{Status: "bad_request"},
		{Status: "no_route", Path: hexBytes("/apx")},
		{Status: "route", Path: hexBytes("/*"), Host: "*", PathPrefix: "/", Cluster: "web"},
	}
	got := as[0].Results
	if len(got) != len(want) {
		t.Fatalf("got %d results", len(got))
	}
	for i := range want {
		g, _ := json.Marshal(got[i])
		w, _ := json.Marshal(want[i])
		if !bytes.Equal(g, w) {
			t.Errorf("request %d: got %s, want %s", i, g, w)
		}
	}

	// A rejected configuration answers like validate, with no results.
	as = ask(t, `{"op":"route","config":"`+hx("{}")+`","requests":[{"host":"h","target":"2f"}]}`)
	if as[0].OK || as[0].Class != "invalid" || as[0].Results != nil {
		t.Errorf("rejected: %+v", as[0])
	}
}

func TestBytesSurviveTheTrip(t *testing.T) {
	// Invalid UTF-8 in a path prefix reaches steer's decoder unchanged and
	// comes back as hex, not as replacement characters in a JSON string.
	d := `{"listen":":80","routes":[{"path_prefix":"/` + "\xff" + `","cluster":"a"}],"clusters":[{"name":"a","endpoints":[{"address":"h:1"}]}]}`
	q, _ := json.Marshal(query{Op: "route", Config: hexBytes(d), Requests: []request{{Host: "h", Target: []byte("/%EF%BF%BD")}}})
	as := ask(t, string(q))
	r := as[0].Results[0]
	if r.Status != "route" || r.PathPrefix != "/�" || string(r.Path) != "/�" {
		t.Errorf("%+v", r)
	}
}

func TestMalformedQueriesStop(t *testing.T) {
	for _, in := range []string{
		"not json",
		`{"op":"nope","config":""}`,
		`{"op":"validate","config":"zz"}`,
		`{"op":"validate","config":5}`,
	} {
		var out bytes.Buffer
		err := serve(strings.NewReader(`{"op":"validate","config":""}`+"\n"+in+"\n"), &out)
		if err == nil || !strings.HasPrefix(err.Error(), "line 2: ") {
			t.Errorf("%s: err = %v", in, err)
		}
		if strings.Count(out.String(), "\n") != 1 {
			t.Errorf("%s: the first query was not answered: %q", in, out.String())
		}
	}
}

func TestClassifyOther(t *testing.T) {
	if c := classify(errors.New("something else")); c != "other" {
		t.Errorf("classify = %q", c)
	}
}

type failWriter struct{}

func (failWriter) Write([]byte) (int, error) { return 0, errors.New("closed") }

type failReader struct{}

func (failReader) Read([]byte) (int, error) { return 0, errors.New("broken") }

func TestIOErrorsStop(t *testing.T) {
	if err := serve(strings.NewReader(`{"op":"validate","config":""}`), failWriter{}); err == nil {
		t.Error("write error ignored")
	}
	if err := serve(failReader{}, &bytes.Buffer{}); err == nil || err.Error() != "broken" {
		t.Errorf("read error: %v", err)
	}
}
