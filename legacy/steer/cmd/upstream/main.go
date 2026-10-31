// Command upstream is a fake upstream for demos and failure drills. It serves
// any path with a small JSON body and takes its faults from a control
// endpoint at run time:
//
//	curl -X PUT localhost:19001/state -d '{"error_rate":0.5}'     # half the requests get 503
//	curl -X PUT localhost:19001/state -d '{"latency":"200ms"}'    # add latency
//	curl -X PUT localhost:19001/state -d '{"blackhole":true}'     # accept and never answer
//	curl localhost:19001/state                                    # settings and counters
//
// /healthz answers 200 subject to latency and blackhole, but not to the error
// rate.
//
// Flags:
//
//	-addr      traffic listen address (default 127.0.0.1:9001)
//	-control   control listen address (default 127.0.0.1:19001)
//	-name      name reported in responses (default: the traffic address)
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/laiwf94/steer/internal/fakeupstream"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, os.Args[1:], os.Stdout, nil); err != nil {
		fmt.Fprintln(os.Stderr, "upstream:", err)
		os.Exit(2)
	}
}

func run(ctx context.Context, args []string, logOut io.Writer, ready func(addr, control string)) error {
	fs := flag.NewFlagSet("upstream", flag.ContinueOnError)
	fs.SetOutput(logOut)
	addr := fs.String("addr", "127.0.0.1:9001", "traffic listen address")
	control := fs.String("control", "127.0.0.1:19001", "control listen address")
	name := fs.String("name", "", "name reported in responses")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() > 0 {
		return fmt.Errorf("unexpected arguments: %v", fs.Args())
	}
	if *name == "" {
		*name = *addr
	}
	log := slog.New(slog.NewJSONHandler(logOut, nil))
	up := fakeupstream.New(*name)
	defer up.Close()

	ln, err := net.Listen("tcp", *addr)
	if err != nil {
		return err
	}
	cln, err := net.Listen("tcp", *control)
	if err != nil {
		ln.Close()
		return err
	}
	srv := &http.Server{Handler: up.Handler(), ReadHeaderTimeout: 5 * time.Second, IdleTimeout: 60 * time.Second}
	csrv := &http.Server{Handler: up.Control(), ReadHeaderTimeout: 5 * time.Second, ReadTimeout: 10 * time.Second}
	log.Info("listening", "name", *name, "addr", ln.Addr().String(), "control", cln.Addr().String())
	if ready != nil {
		ready(ln.Addr().String(), cln.Addr().String())
	}
	errc := make(chan error, 2)
	go func() { errc <- srv.Serve(ln) }()
	go func() { errc <- csrv.Serve(cln) }()
	select {
	case err = <-errc:
	case <-ctx.Done():
	}
	// Close rather than Shutdown: blackholed requests never finish on their own.
	up.Close()
	srv.Close()
	csrv.Close()
	if errors.Is(err, http.ErrServerClosed) {
		err = nil
	}
	return err
}
