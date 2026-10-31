// Command steer runs the HTTP edge load balancer.
//
// Configuration comes from the environment:
//
//	STEER_CONFIG            path to the JSON configuration file (required)
//	STEER_ADMIN_TOKEN       bearer token for the admin API, at least 16 characters;
//	                        required when admin_listen is not a loopback address
//	STEER_DRAIN_PERIOD      how long /readyz reports draining before shutdown starts (default 5s)
//	STEER_SHUTDOWN_TIMEOUT  how long in-flight requests get to finish after that (default 30s)
//	STEER_LOG_LEVEL         debug, info, warn or error (default info)
//
// With a "tls" block in the configuration the proxy listener terminates TLS.
//
// SIGHUP, or POST /reload on the admin listener, re-reads the configuration
// file, including certificates and keys. SIGTERM or SIGINT drains: /readyz turns 503 so upstream load balancers
// stop sending traffic, keep-alive is switched off, and after the drain period
// the listeners close and in-flight requests are given time to finish.
package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/netip"
	"os"
	"os/signal"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/laiwf94/steer/internal/admin"
	"github.com/laiwf94/steer/internal/config"
	"github.com/laiwf94/steer/internal/metrics"
	"github.com/laiwf94/steer/internal/proxy"
	"github.com/laiwf94/steer/internal/tlsedge"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	hup := make(chan os.Signal, 1)
	signal.Notify(hup, syscall.SIGHUP)
	if err := run(ctx, os.Getenv, os.Stdout, hup, nil); err != nil {
		fmt.Fprintln(os.Stderr, "steer:", err)
		os.Exit(1)
	}
}

type settings struct {
	configPath      string
	adminToken      string
	drainPeriod     time.Duration
	shutdownTimeout time.Duration
	logLevel        slog.Level
}

func loadSettings(getenv func(string) string) (settings, error) {
	s := settings{configPath: getenv("STEER_CONFIG"), adminToken: getenv("STEER_ADMIN_TOKEN"),
		drainPeriod: 5 * time.Second, shutdownTimeout: 30 * time.Second}
	if s.configPath == "" {
		return s, errors.New("STEER_CONFIG is required (path to the JSON configuration file)")
	}
	if s.adminToken != "" && len(s.adminToken) < admin.MinTokenLength {
		return s, fmt.Errorf("STEER_ADMIN_TOKEN must be at least %d characters", admin.MinTokenLength)
	}
	for _, d := range []struct {
		name string
		dst  *time.Duration
		max  time.Duration
	}{{"STEER_DRAIN_PERIOD", &s.drainPeriod, 10 * time.Minute}, {"STEER_SHUTDOWN_TIMEOUT", &s.shutdownTimeout, time.Hour}} {
		if v := getenv(d.name); v != "" {
			n, err := time.ParseDuration(v)
			if err != nil || n < 0 || n > d.max {
				return s, fmt.Errorf("%s must be a duration between 0s and %s", d.name, d.max)
			}
			*d.dst = n
		}
	}
	if v := getenv("STEER_LOG_LEVEL"); v != "" {
		if err := s.logLevel.UnmarshalText([]byte(v)); err != nil {
			return s, fmt.Errorf("STEER_LOG_LEVEL: %v", err)
		}
	}
	return s, nil
}

// loopback reports whether a listen address only accepts local connections.
func loopback(addr string) bool {
	host, _, err := net.SplitHostPort(addr)
	if err != nil {
		return false
	}
	if host == "localhost" {
		return true
	}
	a, err := netip.ParseAddr(host)
	return err == nil && a.IsLoopback()
}

func loadConfig(s settings) (*config.Config, error) {
	cfg, err := config.Load(s.configPath)
	if err != nil {
		return nil, err
	}
	if s.adminToken == "" && !loopback(cfg.AdminListen) {
		return nil, fmt.Errorf("admin_listen %s is not a loopback address; set STEER_ADMIN_TOKEN to expose the admin API", cfg.AdminListen)
	}
	return cfg, nil
}

// loadTLS reads cfg's certificates and logs anything about them that will
// not stop them serving but needs an operator's attention.
func loadTLS(cfg *config.Config, log *slog.Logger) (*tlsedge.Snapshot, error) {
	snap, err := tlsedge.Load(cfg.TLS)
	if err != nil {
		return nil, err
	}
	for _, w := range snap.Warnings(time.Now()) {
		log.Warn("tls certificate", "problem", w)
	}
	return snap, nil
}

// run serves until ctx ends, then drains and shuts down. hup delivers reload
// requests. ready, when set, receives the bound proxy and admin addresses.
func run(ctx context.Context, getenv func(string) string, logOut io.Writer, hup <-chan os.Signal, ready func(proxyAddr, adminAddr string)) error {
	s, err := loadSettings(getenv)
	if err != nil {
		return err
	}
	log := slog.New(slog.NewJSONHandler(logOut, &slog.HandlerOptions{Level: s.logLevel}))
	cfg, err := loadConfig(s)
	if err != nil {
		return err
	}
	reg := metrics.New()
	var tlsStore *tlsedge.Store
	if cfg.TLS != nil {
		snap, err := loadTLS(cfg, log)
		if err != nil {
			return err
		}
		tlsStore = tlsedge.NewStore(snap)
	}
	p, err := proxy.New(cfg, proxy.Options{Logger: log, Metrics: reg})
	if err != nil {
		return err
	}
	defer p.Close()

	var reloadMu sync.Mutex
	reload := func() (uint64, error) {
		reloadMu.Lock()
		defer reloadMu.Unlock()
		next, err := loadConfig(s)
		var snap *tlsedge.Snapshot
		if err == nil && (next.TLS == nil) != (tlsStore == nil) {
			err = errors.New("tls cannot be turned on or off by a reload; restart steer")
		}
		if err == nil && next.TLS != nil {
			snap, err = loadTLS(next, log)
		}
		if err != nil {
			p.RecordFailedReload()
		} else if err = p.Reload(next); err == nil && snap != nil {
			// Handshakes from now on use the new certificates; established
			// connections keep the ones they negotiated.
			tlsStore.Swap(snap)
		}
		if err != nil {
			log.Warn("configuration rejected; the previous configuration keeps serving", "error", err.Error())
			return 0, err
		}
		return p.State().ConfigVersion, nil
	}
	var draining atomic.Bool
	adm, err := admin.New(admin.Config{Token: s.adminToken, Logger: log, Metrics: reg,
		State: func() any { return p.State() }, Reload: reload, Ready: func() bool { return !draining.Load() }})
	if err != nil {
		return err
	}

	var proxyLn net.Listener
	proxyLn, err = net.Listen("tcp", cfg.Listen)
	if err != nil {
		return err
	}
	if tlsStore != nil {
		results := map[string]*metrics.Counter{}
		for _, r := range []string{tlsedge.ResultOK, tlsedge.ResultTimeout, tlsedge.ResultError} {
			results[r] = reg.Counter("steer_tls_handshakes_total", `result="`+r+`"`, "TLS handshakes on the proxy listener by result.")
		}
		duration := reg.Histogram("steer_tls_handshake_duration_seconds", "", "Time to complete a TLS handshake on the proxy listener.")
		tl := tlsedge.NewListener(proxyLn, tlsStore, func(result string, d time.Duration) {
			results[result].Inc()
			if result == tlsedge.ResultOK {
				duration.Observe(d)
			}
		})
		reg.Gauge("steer_tls_handshakes_in_progress", "TLS handshakes running on the proxy listener.", func() float64 { return float64(tl.InProgress()) })
		defer tl.Wait()
		proxyLn = tl
	}
	adminLn, err := net.Listen("tcp", cfg.AdminListen)
	if err != nil {
		proxyLn.Close()
		return err
	}
	errorLog := slog.NewLogLogger(log.Handler(), slog.LevelWarn)
	proxySrv := &http.Server{
		Handler:           p,
		ReadHeaderTimeout: 10 * time.Second,
		IdleTimeout:       120 * time.Second,
		MaxHeaderBytes:    64 << 10,
		ErrorLog:          errorLog,
		// No ReadTimeout or WriteTimeout: they would cut long uploads and
		// streams. Each cluster's timeout bounds a request instead.
	}
	adminSrv := &http.Server{
		Handler:           adm,
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       10 * time.Second,
		WriteTimeout:      30 * time.Second,
		IdleTimeout:       60 * time.Second,
		MaxHeaderBytes:    16 << 10,
		ErrorLog:          errorLog,
	}
	log.Info("listening", "proxy", proxyLn.Addr().String(), "tls", tlsStore != nil, "admin", adminLn.Addr().String(),
		"admin_auth", s.adminToken != "", "clusters", len(cfg.Clusters), "routes", len(cfg.Routes))
	if ready != nil {
		ready(proxyLn.Addr().String(), adminLn.Addr().String())
	}
	errc := make(chan error, 2)
	go func() { errc <- proxySrv.Serve(proxyLn) }()
	go func() { errc <- adminSrv.Serve(adminLn) }()

	for running := true; running; {
		select {
		case err := <-errc:
			proxySrv.Close()
			adminSrv.Close()
			return err
		case <-hup:
			if v, err := reload(); err == nil {
				log.Info("reloaded on SIGHUP", "version", v)
			}
		case <-ctx.Done():
			running = false
		}
	}

	draining.Store(true)
	proxySrv.SetKeepAlivesEnabled(false)
	log.Info("draining", "drain_period_ms", s.drainPeriod.Milliseconds())
	time.Sleep(s.drainPeriod)
	log.Info("shutting down", "timeout_ms", s.shutdownTimeout.Milliseconds())
	shutdownCtx, cancel := context.WithTimeout(context.Background(), s.shutdownTimeout)
	defer cancel()
	for name, srv := range map[string]*http.Server{"proxy": proxySrv, "admin": adminSrv} {
		if err := srv.Shutdown(shutdownCtx); err != nil {
			// Connections still open at the deadline (including ones a client
			// opened but never used) are cut; shutdown itself has succeeded.
			log.Warn("shutdown timeout reached; closing remaining connections", "listener", name, "error", err.Error())
			srv.Close()
		}
	}
	log.Info("stopped")
	return nil
}
