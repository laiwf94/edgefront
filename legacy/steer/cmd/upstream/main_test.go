package main

import (
	"context"
	"io"
	"net"
	"net/http"
	"strings"
	"testing"
	"time"
)

func TestRun(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	addrs := make(chan [2]string, 1)
	done := make(chan error, 1)
	go func() {
		done <- run(ctx, []string{"-addr", "127.0.0.1:0", "-control", "127.0.0.1:0", "-name", "alpha"}, io.Discard,
			func(a, c string) { addrs <- [2]string{a, c} })
	}()
	ad := <-addrs
	resp, err := http.Get("http://" + ad[0] + "/hello")
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if !strings.Contains(string(b), `"upstream":"alpha"`) {
		t.Fatalf("body %s", b)
	}
	req, _ := http.NewRequest(http.MethodPut, "http://"+ad[1]+"/state", strings.NewReader(`{"blackhole":true}`))
	resp, err = http.DefaultClient.Do(req)
	if err != nil || resp.StatusCode != 200 {
		t.Fatalf("control: %v %v", resp, err)
	}
	resp.Body.Close()
	client := &http.Client{Timeout: 50 * time.Millisecond}
	if _, err := client.Get("http://" + ad[0] + "/"); err == nil {
		t.Fatal("blackhole answered")
	}
	// A held request must not block shutdown.
	go http.Get("http://" + ad[0] + "/held")
	time.Sleep(20 * time.Millisecond)
	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("shutdown blocked by a blackholed request")
	}
}

func TestRunErrors(t *testing.T) {
	ln, _ := net.Listen("tcp", "127.0.0.1:0")
	defer ln.Close()
	cases := map[string][]string{
		"flag provided but not defined": {"-nope"},
		"unexpected arguments":          {"extra"},
		"address already in use":        {"-addr", ln.Addr().String()},
	}
	for want, args := range cases {
		if err := run(context.Background(), args, io.Discard, nil); err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%v: err %v, want %q", args, err, want)
		}
	}
	err := run(context.Background(), []string{"-addr", "127.0.0.1:0", "-control", ln.Addr().String()}, io.Discard, nil)
	if err == nil || !strings.Contains(err.Error(), "address already in use") {
		t.Fatalf("control clash: %v", err)
	}
}
