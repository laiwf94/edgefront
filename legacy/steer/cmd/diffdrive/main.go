// Command diffdrive answers configuration and routing queries with steer's
// own code, so edgefront's Rust port can be diffed against it.
//
// It reads one JSON query per line on stdin and writes one JSON answer per
// line on stdout, in order. Byte strings (configuration documents and request
// targets) travel as hex so that any bytes, including invalid UTF-8, arrive
// unchanged.
//
//	{"op":"validate","config":"7b...7d"}
//	{"ok":true}
//	{"ok":false,"class":"invalid","error":"invalid configuration:\n..."}
//
//	{"op":"route","config":"7b...7d","requests":[{"host":"a.example","target":"2f78"}]}
//	{"ok":true,"results":[{"status":"route","path":"2f78","host":"a.example","path_prefix":"/","cluster":"web"}]}
//
// A route query first validates its configuration; when that fails the answer
// is the validate answer and there are no results. Each request is decided the
// way steer's server decided it: the target goes through url.ParseRequestURI
// (net/http answered 400 when that failed), its path through route.CleanPath,
// and the cleaned path and Host through the router built from the
// configuration's routes. Callers send origin-form targets and "*": an
// absolute-form target (one with a scheme) is parsed for its path here,
// which the Rust side leaves to its caller, so it is not a useful query.
//
// The class of a rejection is "size" (over config.MaxFileBytes), "json" (the
// decoder refused the document) or "invalid" (validation found problems; the
// error then lists them).
package main

import (
	"bufio"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"strings"

	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/route"
)

func main() {
	if err := serve(os.Stdin, os.Stdout); err != nil {
		fmt.Fprintln(os.Stderr, "diffdrive:", err)
		os.Exit(1)
	}
}

type query struct {
	Op       string    `json:"op"`
	Config   hexBytes  `json:"config"`
	Requests []request `json:"requests"`
}

type request struct {
	Host   string   `json:"host"`
	Target hexBytes `json:"target"`
}

type answer struct {
	OK      bool     `json:"ok"`
	Class   string   `json:"class,omitempty"`
	Error   string   `json:"error,omitempty"`
	Results []result `json:"results,omitempty"`
}

type result struct {
	Status     string   `json:"status"` // bad_request, no_route or route
	Path       hexBytes `json:"path,omitempty"`
	Host       string   `json:"host,omitempty"`
	PathPrefix string   `json:"path_prefix,omitempty"`
	Cluster    string   `json:"cluster,omitempty"`
}

// hexBytes is a byte string written as lower-case hex in JSON.
type hexBytes []byte

func (h hexBytes) MarshalJSON() ([]byte, error) {
	return json.Marshal(hex.EncodeToString(h))
}

func (h *hexBytes) UnmarshalJSON(b []byte) error {
	var s string
	if err := json.Unmarshal(b, &s); err != nil {
		return err
	}
	d, err := hex.DecodeString(s)
	if err != nil {
		return err
	}
	*h = d
	return nil
}

// serve answers queries from r on w until r ends. A malformed query line or a
// failed write stops it with an error.
func serve(r io.Reader, w io.Writer) error {
	in := bufio.NewReader(r)
	out := bufio.NewWriter(w)
	enc := json.NewEncoder(out)
	for line := 1; ; line++ {
		b, err := in.ReadBytes('\n')
		if len(b) == 0 && errors.Is(err, io.EOF) {
			return out.Flush()
		}
		if err != nil && !errors.Is(err, io.EOF) {
			return err
		}
		var q query
		if err := json.Unmarshal(b, &q); err != nil {
			return fmt.Errorf("line %d: %w", line, err)
		}
		a, err := handle(&q)
		if err != nil {
			return fmt.Errorf("line %d: %w", line, err)
		}
		if err := enc.Encode(a); err != nil {
			return err
		}
		// Answer each query as it comes, so a caller can keep one query in
		// flight without deadlocking on a full buffer.
		if err := out.Flush(); err != nil {
			return err
		}
	}
}

func handle(q *query) (answer, error) {
	switch q.Op {
	case "validate":
		a, _ := validate(q.Config)
		return a, nil
	case "route":
		a, cfg := validate(q.Config)
		if cfg == nil {
			return a, nil
		}
		router := route.New(cfg.Routes)
		a.Results = make([]result, 0, len(q.Requests))
		for _, req := range q.Requests {
			a.Results = append(a.Results, decide(router, req))
		}
		return a, nil
	default:
		return answer{}, fmt.Errorf("unknown op %q", q.Op)
	}
}

func validate(doc []byte) (answer, *config.Config) {
	cfg, err := config.Parse(doc)
	if err != nil {
		return answer{Class: classify(err), Error: err.Error()}, nil
	}
	return answer{OK: true}, cfg
}

func classify(err error) string {
	msg := err.Error()
	switch {
	case strings.HasPrefix(msg, "invalid configuration:"):
		return "invalid"
	case strings.HasPrefix(msg, "invalid JSON"):
		return "json"
	case strings.HasPrefix(msg, "configuration is larger than"):
		return "size"
	default:
		return "other"
	}
}

func decide(router *route.Router, req request) result {
	u, err := url.ParseRequestURI(string(req.Target))
	if err != nil {
		return result{Status: "bad_request"}
	}
	p := route.CleanPath(u.Path)
	rt, ok := router.Match(req.Host, p)
	if !ok {
		return result{Status: "no_route", Path: hexBytes(p)}
	}
	return result{Status: "route", Path: hexBytes(p), Host: rt.Host, PathPrefix: rt.PathPrefix, Cluster: rt.Cluster}
}
