package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

type logBuf struct {
	mu sync.Mutex
	b  bytes.Buffer
}

func (l *logBuf) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.b.Write(p)
}

func (l *logBuf) String() string {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.b.String()
}

func upstream(t *testing.T, name string) string {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		io.WriteString(w, name)
	}))
	t.Cleanup(srv.Close)
	return srv.Listener.Addr().String()
}

func writeConfig(t *testing.T, path, adminAddr, upstreamAddr string) {
	t.Helper()
	js := fmt.Sprintf(`{"listen":"127.0.0.1:0","admin_listen":%q,"routes":[{"cluster":"web"}],
		"clusters":[{"name":"web","endpoints":[{"address":%q}]}]}`, adminAddr, upstreamAddr)
	if err := os.WriteFile(path, []byte(js), 0o600); err != nil {
		t.Fatal(err)
	}
}

func env(m map[string]string) func(string) string { return func(k string) string { return m[k] } }

func get(t *testing.T, url, token string) (int, string) {
	t.Helper()
	req, _ := http.NewRequest(http.MethodGet, url, nil)
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return 0, err.Error()
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(b)
}

func TestRunReloadAndDrain(t *testing.T) {
	dir := t.TempDir()
	cfgPath := filepath.Join(dir, "steer.json")
	a, b := upstream(t, "a"), upstream(t, "b")
	writeConfig(t, cfgPath, "127.0.0.1:0", a)
	const token = "admin-token-0123456789"

	ctx, cancel := context.WithCancel(context.Background())
	hup := make(chan os.Signal, 1)
	addrs := make(chan [2]string, 1)
	done := make(chan error, 1)
	logs := &logBuf{}
	go func() {
		done <- run(ctx, env(map[string]string{"STEER_CONFIG": cfgPath, "STEER_ADMIN_TOKEN": token,
			"STEER_DRAIN_PERIOD": "300ms", "STEER_SHUTDOWN_TIMEOUT": "2s", "STEER_LOG_LEVEL": "debug"}), logs, hup,
			func(p, a string) { addrs <- [2]string{p, a} })
	}()
	var ad [2]string
	select {
	case ad = <-addrs:
	case err := <-done:
		t.Fatal(err)
	}
	proxyURL, adminURL := "http://"+ad[0], "http://"+ad[1]

	if code, body := get(t, proxyURL+"/", ""); code != 200 || body != "a" {
		t.Fatalf("proxy: %d %q", code, body)
	}
	if code, _ := get(t, adminURL+"/clusters", ""); code != 401 {
		t.Fatalf("admin without token: %d", code)
	}
	if code, body := get(t, adminURL+"/clusters", token); code != 200 || !strings.Contains(body, a) {
		t.Fatalf("clusters: %d %s", code, body)
	}

	// SIGHUP reload switches the upstream.
	writeConfig(t, cfgPath, "127.0.0.1:0", b)
	hup <- os.Interrupt
	deadline := time.Now().Add(5 * time.Second)
	for {
		if _, body := get(t, proxyURL+"/", ""); body == "b" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("SIGHUP reload did not take effect")
		}
		time.Sleep(10 * time.Millisecond)
	}

	// A broken file is rejected over the admin API and the old config serves.
	os.WriteFile(cfgPath, []byte(`{"listen":`), 0o600)
	req, _ := http.NewRequest(http.MethodPost, adminURL+"/reload", nil)
	req.Header.Set("Authorization", "Bearer "+token)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	var out map[string]map[string]string
	json.NewDecoder(resp.Body).Decode(&out)
	resp.Body.Close()
	if resp.StatusCode != 400 || out["error"]["code"] != "invalid_config" {
		t.Fatalf("bad reload: %d %v", resp.StatusCode, out)
	}
	hup <- os.Interrupt // also rejected; must not stop the server
	if _, body := get(t, proxyURL+"/", ""); body != "b" {
		t.Fatalf("old config stopped serving: %q", body)
	}
	deadline = time.Now().Add(5 * time.Second)
	for {
		_, metrics := get(t, adminURL+"/metrics", token)
		if strings.Contains(metrics, `steer_config_reloads_total{result="failure"} 2`) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("failed reloads not counted:\n%s", metrics)
		}
		time.Sleep(5 * time.Millisecond)
	}
	// A good file via the admin API.
	writeConfig(t, cfgPath, "127.0.0.1:0", a)
	req, _ = http.NewRequest(http.MethodPost, adminURL+"/reload", nil)
	req.Header.Set("Authorization", "Bearer "+token)
	resp, err = http.DefaultClient.Do(req)
	if err != nil || resp.StatusCode != 200 {
		t.Fatalf("good reload: %v %v", resp, err)
	}
	resp.Body.Close()

	// Drain: readyz flips to 503 while the proxy still answers, then both stop.
	cancel()
	deadline = time.Now().Add(5 * time.Second)
	for {
		if code, _ := get(t, adminURL+"/readyz", ""); code == 503 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("readyz never reported draining")
		}
		time.Sleep(5 * time.Millisecond)
	}
	if code, body := get(t, proxyURL+"/", ""); code != 200 || body != "a" {
		t.Fatalf("proxy during drain: %d %q", code, body)
	}
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("did not shut down")
	}
	if code, _ := get(t, proxyURL+"/", ""); code != 0 {
		t.Fatal("proxy still listening after shutdown")
	}
	for _, want := range []string{`"msg":"listening"`, `"msg":"reloaded on SIGHUP"`, `"msg":"configuration rejected; the previous configuration keeps serving"`, `"msg":"draining"`, `"msg":"stopped"`} {
		if !strings.Contains(logs.String(), want) {
			t.Errorf("log lacks %s", want)
		}
	}
}

func TestStartupErrors(t *testing.T) {
	dir := t.TempDir()
	good := filepath.Join(dir, "good.json")
	writeConfig(t, good, "127.0.0.1:0", "127.0.0.1:1")
	public := filepath.Join(dir, "public.json")
	writeConfig(t, public, "0.0.0.0:0", "127.0.0.1:1")
	busy, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer busy.Close()
	clash := filepath.Join(dir, "clash.json")
	os.WriteFile(clash, []byte(fmt.Sprintf(`{"listen":%q,"routes":[{"cluster":"w"}],"clusters":[{"name":"w","endpoints":[{"address":"h:1"}]}]}`, busy.Addr().String())), 0o600)
	adminClash := filepath.Join(dir, "adminclash.json")
	writeConfig(t, adminClash, busy.Addr().String(), "127.0.0.1:1")

	cases := []struct {
		env  map[string]string
		want string
	}{
		{map[string]string{}, "STEER_CONFIG is required"},
		{map[string]string{"STEER_CONFIG": filepath.Join(dir, "missing.json")}, "no such file"},
		{map[string]string{"STEER_CONFIG": good, "STEER_ADMIN_TOKEN": "short"}, "at least 16"},
		{map[string]string{"STEER_CONFIG": good, "STEER_DRAIN_PERIOD": "forever"}, "STEER_DRAIN_PERIOD"},
		{map[string]string{"STEER_CONFIG": good, "STEER_SHUTDOWN_TIMEOUT": "-1s"}, "STEER_SHUTDOWN_TIMEOUT"},
		{map[string]string{"STEER_CONFIG": good, "STEER_LOG_LEVEL": "loud"}, "STEER_LOG_LEVEL"},
		{map[string]string{"STEER_CONFIG": public}, "not a loopback address"},
		{map[string]string{"STEER_CONFIG": clash}, "address already in use"},
		{map[string]string{"STEER_CONFIG": adminClash}, "address already in use"},
	}
	for _, c := range cases {
		err := run(context.Background(), env(c.env), io.Discard, nil, nil)
		if err == nil || !strings.Contains(err.Error(), c.want) {
			t.Errorf("env %v: err %v, want %q", c.env, err, c.want)
		}
	}
	// A public admin address is fine with a token.
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	err = run(ctx, env(map[string]string{"STEER_CONFIG": public, "STEER_ADMIN_TOKEN": "admin-token-0123456789",
		"STEER_DRAIN_PERIOD": "0s"}), io.Discard, nil, nil)
	if err != nil {
		t.Fatalf("public admin with token: %v", err)
	}
}

func TestLoopback(t *testing.T) {
	for addr, want := range map[string]bool{"127.0.0.1:9901": true, "[::1]:9901": true, "localhost:1": true,
		":9901": false, "0.0.0.0:1": false, "10.0.0.1:1": false, "garbage": false} {
		if got := loopback(addr); got != want {
			t.Errorf("loopback(%q) = %v", addr, got)
		}
	}
}
