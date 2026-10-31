package tlsedge

import (
	"context"
	"crypto/tls"
	"errors"
	"net"
	"sync"
	"sync/atomic"
	"time"
)

// Handshake results passed to the callback given to NewListener.
const (
	ResultOK      = "ok"
	ResultTimeout = "timeout" // the handshake did not finish within the timeout
	ResultError   = "error"   // protocol error, no shared version or suite, unknown SNI under strict
)

// Listener accepts TCP connections and returns them as *tls.Conn values whose
// handshake has already completed.
//
// Handshakes run in their own goroutines, off the accept loop, so a client
// that connects and then stalls cannot hold up anyone else. Each one has the
// snapshot's handshake timeout as a hard deadline on the socket, which is
// cleared once the handshake succeeds; after that the HTTP server's own
// timeouts apply. At most MaxConcurrentHandshakes run at once; past that the
// accept loop waits, and new connections queue in the kernel's backlog
// rather than in steer's memory.
type Listener struct {
	inner       net.Listener
	store       *Store
	onHandshake func(result string, d time.Duration)

	ready  chan net.Conn
	ctx    context.Context
	cancel context.CancelFunc
	start  sync.Once
	closed sync.Once
	wg     sync.WaitGroup

	errMu  sync.Mutex
	err    error         // set when the inner listener fails for good
	failed chan struct{} // closed with err

	mu      sync.Mutex
	cond    *sync.Cond
	running int // slots taken: handshakes plus the one the accept loop holds while it waits

	active atomic.Int64 // handshakes in progress
}

// NewListener wraps inner. Closing the returned listener closes inner.
// onHandshake, if not nil, is called after every handshake that ends before
// the listener is closed, with its result (ResultOK, ResultTimeout or
// ResultError) and duration. It must be safe for concurrent use.
func NewListener(inner net.Listener, store *Store, onHandshake func(result string, d time.Duration)) *Listener {
	ctx, cancel := context.WithCancel(context.Background())
	l := &Listener{inner: inner, store: store, onHandshake: onHandshake, ready: make(chan net.Conn),
		ctx: ctx, cancel: cancel, failed: make(chan struct{})}
	l.cond = sync.NewCond(&l.mu)
	return l
}

// Accept returns the next connection whose TLS handshake succeeded.
func (l *Listener) Accept() (net.Conn, error) {
	l.start.Do(func() {
		l.wg.Add(1)
		go l.acceptLoop()
	})
	select {
	case c := <-l.ready:
		return c, nil
	case <-l.ctx.Done():
		return nil, net.ErrClosed
	case <-l.failed:
		l.errMu.Lock()
		defer l.errMu.Unlock()
		return nil, l.err
	}
}

// Close stops accepting, closes the inner listener and abandons handshakes
// in progress. Connections already returned by Accept are not affected.
func (l *Listener) Close() error {
	var err error
	l.closed.Do(func() {
		l.cancel()
		err = l.inner.Close()
		l.mu.Lock()
		l.cond.Broadcast()
		l.mu.Unlock()
	})
	return err
}

// Wait blocks until the accept loop and every handshake goroutine have
// returned. Call it after Close.
func (l *Listener) Wait() { l.wg.Wait() }

// Addr returns the inner listener's address.
func (l *Listener) Addr() net.Addr { return l.inner.Addr() }

// InProgress returns the number of handshakes running now.
func (l *Listener) InProgress() int { return int(l.active.Load()) }

func (l *Listener) acceptLoop() {
	defer l.wg.Done()
	var backoff time.Duration
	for {
		if !l.acquire() {
			return
		}
		raw, err := l.inner.Accept()
		if err != nil {
			l.release()
			if l.ctx.Err() != nil {
				return
			}
			var ne net.Error
			if errors.As(err, &ne) && ne.Timeout() || isTemporary(err) {
				// Out of file descriptors and the like: back off as
				// net/http does instead of spinning.
				backoff = min(max(2*backoff, 5*time.Millisecond), time.Second)
				select {
				case <-time.After(backoff):
				case <-l.ctx.Done():
					return
				}
				continue
			}
			l.errMu.Lock()
			l.err = err
			l.errMu.Unlock()
			close(l.failed)
			return
		}
		backoff = 0
		l.active.Add(1)
		l.wg.Add(1)
		go l.handshake(raw)
	}
}

func isTemporary(err error) bool {
	t, ok := err.(interface{ Temporary() bool })
	return ok && t.Temporary()
}

// acquire waits for a handshake slot. It returns false once the listener is
// closed.
func (l *Listener) acquire() bool {
	l.mu.Lock()
	defer l.mu.Unlock()
	for l.ctx.Err() == nil && l.running >= l.store.Current().MaxConcurrentHandshakes {
		l.cond.Wait()
	}
	if l.ctx.Err() != nil {
		return false
	}
	l.running++
	return true
}

func (l *Listener) release() {
	l.mu.Lock()
	l.running--
	l.cond.Signal()
	l.mu.Unlock()
}

func (l *Listener) handshake(raw net.Conn) {
	defer l.wg.Done()
	snap := l.store.Current()
	start := time.Now()
	deadline := start.Add(snap.HandshakeTimeout)
	raw.SetDeadline(deadline)
	tc := tls.Server(raw, l.store.Config())
	ctx, cancel := context.WithDeadline(l.ctx, deadline)
	err := tc.HandshakeContext(ctx)
	cancel()
	l.active.Add(-1)
	l.release()
	if err != nil {
		raw.Close()
		if l.ctx.Err() != nil {
			return
		}
		result := ResultError
		var ne net.Error
		if errors.Is(err, context.DeadlineExceeded) || errors.As(err, &ne) && ne.Timeout() {
			result = ResultTimeout
		}
		l.report(result, time.Since(start))
		return
	}
	raw.SetDeadline(time.Time{})
	l.report(ResultOK, time.Since(start))
	select {
	case l.ready <- tc:
	case <-l.ctx.Done():
		tc.Close()
	}
}

func (l *Listener) report(result string, d time.Duration) {
	if l.onHandshake != nil {
		l.onHandshake(result, d)
	}
}
