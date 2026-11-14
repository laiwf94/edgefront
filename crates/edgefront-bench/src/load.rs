//! The open-loop load generator.
//!
//! A load generator that waits for each response before sending the next
//! request measures the server at whatever rate the server allows: when the
//! server stalls, the generator stalls with it, the requests it would have
//! sent are never sent, and the stall shows up as one slow sample instead
//! of the many a real client population would have seen. That is
//! coordinated omission.
//!
//! This one sends on a fixed schedule instead. A [`Schedule`] is a list of
//! [`Phase`]s, each a constant arrival rate held for a duration (a stepped
//! schedule is several phases), and request `k` of a phase is due exactly
//! `k / rate` seconds after the phase starts, whatever happened to earlier
//! requests. A dispatcher hands each request to the connection workers at
//! its due time; a request that finds every connection busy waits in a
//! queue, and its latency is measured from the time it was due (its
//! *response time*), so the wait counts. The time from the moment it was
//! written to the connection is recorded too (its *service time*): the two
//! differ exactly by the queueing a closed-loop tool would have hidden.
//!
//! Over HTTP/1.1 each of `connections` workers owns one connection and
//! sends one request at a time on it. Over HTTP/2 each worker owns one
//! connection and multiplexes every request it takes as a stream of its
//! own, up to the server's concurrent stream limit. A connection that fails
//! is replaced on the next request.
//!
//! A request whose response has not fully arrived `timeout` after it was
//! due is counted as a timeout and kept out of the histograms, as is any
//! other failure; [`Errors`] counts them by kind.

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::uri::{Authority, PathAndQuery, Scheme};
use http::{HeaderValue, Request, Response, Uri, Version, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Instant, sleep_until, timeout_at};

use crate::histogram::Histogram;

/// The HTTP version the generator speaks, over cleartext TCP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP/1.1, one request at a time per connection.
    Http1,
    /// HTTP/2 with prior knowledge, many streams per connection.
    Http2,
}

/// A constant arrival rate held for a duration.
#[derive(Clone, Debug, PartialEq)]
pub struct Phase {
    /// The phase's name in reports.
    pub name: String,
    /// Requests per second; 0 is a pause.
    pub rate: f64,
    /// How long the phase lasts.
    pub duration: Duration,
    /// Whether the phase is warm-up, left out of [`RunReport::measured`].
    pub warmup: bool,
}

impl Phase {
    /// A measured phase.
    pub fn new(name: impl Into<String>, rate: f64, duration: Duration) -> Self {
        Phase {
            name: name.into(),
            rate,
            duration,
            warmup: false,
        }
    }

    /// The same phase, marked as warm-up.
    pub fn warmup(self) -> Self {
        Phase {
            warmup: true,
            ..self
        }
    }

    /// How many requests the phase sends: one every `1 / rate` seconds
    /// from its start, the first at its start, none at its end.
    pub fn requests(&self) -> u64 {
        let exact = self.rate * self.duration.as_secs_f64();
        // Up to the last request due strictly before the end, tolerating
        // the rounding in rate × duration.
        let whole = exact.round();
        if (exact - whole).abs() < 1e-9 {
            whole as u64
        } else {
            exact.ceil() as u64
        }
    }

    /// When request `k` is due, from the start of the phase.
    fn offset(&self, k: u64) -> Duration {
        Duration::from_secs_f64(k as f64 / self.rate)
    }
}

/// The phases of a run, back to back.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    phases: Vec<Phase>,
}

impl Schedule {
    /// Any list of phases.
    pub fn new(phases: Vec<Phase>) -> Self {
        Schedule { phases }
    }

    /// One phase at a constant rate.
    pub fn constant(rate: f64, duration: Duration) -> Self {
        Schedule::new(vec![Phase::new("constant", rate, duration)])
    }

    /// `steps` phases of `step_duration` each, the first at `start` requests
    /// per second and each later one `step` higher, named `step-1` onward.
    pub fn stepped(start: f64, step: f64, steps: usize, step_duration: Duration) -> Self {
        Schedule::new(
            (0..steps)
                .map(|i| {
                    Phase::new(
                        format!("step-{}", i + 1),
                        start + step * i as f64,
                        step_duration,
                    )
                })
                .collect(),
        )
    }

    /// The same schedule after a warm-up phase.
    pub fn with_warmup(mut self, rate: f64, duration: Duration) -> Self {
        self.phases
            .insert(0, Phase::new("warmup", rate, duration).warmup());
        self
    }

    /// The phases in order.
    pub fn phases(&self) -> &[Phase] {
        &self.phases
    }

    /// The sum of the phases' durations.
    pub fn duration(&self) -> Duration {
        self.phases.iter().map(|p| p.duration).sum()
    }
}

/// Builds the request with a sequence number: 0 for the first request of
/// the run, counting across phases. Only the method, path and query,
/// headers and body are used; the generator sets the version, authority
/// and (over HTTP/1.1) the `Host` header unless the request has one.
pub trait RequestSource: Send + Sync + 'static {
    /// The request numbered `seq`.
    fn request(&self, seq: u64) -> Request<Full<Bytes>>;
}

impl<F> RequestSource for F
where
    F: Fn(u64) -> Request<Full<Bytes>> + Send + Sync + 'static,
{
    fn request(&self, seq: u64) -> Request<Full<Bytes>> {
        self(seq)
    }
}

/// A `GET` of one path, the same for every request.
#[derive(Clone, Debug)]
pub struct Get(pub String);

impl RequestSource for Get {
    fn request(&self, _seq: u64) -> Request<Full<Bytes>> {
        let mut request = Request::new(Full::new(Bytes::new()));
        if let Ok(uri) = self.0.parse() {
            *request.uri_mut() = uri;
        }
        request
    }
}

/// What to load, how and how hard.
#[derive(Clone, Debug)]
pub struct LoadConfig {
    /// The address to connect to.
    pub addr: SocketAddr,
    /// The `Host` header or `:authority`; [`LoadConfig::new`] uses the
    /// address.
    pub authority: String,
    /// The HTTP version.
    pub protocol: Protocol,
    /// How many connections to open (one worker each).
    pub connections: usize,
    /// How long after its due time a request may take before it counts
    /// as a timeout.
    pub timeout: Duration,
    /// The arrival schedule.
    pub schedule: Schedule,
}

impl LoadConfig {
    /// Eight connections, a ten-second timeout and the address as the
    /// authority.
    pub fn new(addr: SocketAddr, protocol: Protocol, schedule: Schedule) -> Self {
        LoadConfig {
            addr,
            authority: addr.to_string(),
            protocol,
            connections: 8,
            timeout: Duration::from_secs(10),
            schedule,
        }
    }
}

/// Why a run could not start.
#[derive(Clone, Debug, PartialEq)]
pub enum LoadError {
    /// The schedule has no phases.
    NoPhases,
    /// A phase's rate is negative, infinite or not a number.
    InvalidRate {
        /// The phase's name.
        phase: String,
    },
    /// `connections` is zero.
    NoConnections,
    /// `timeout` is zero.
    ZeroTimeout,
    /// `authority` is not a valid authority.
    InvalidAuthority(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NoPhases => write!(f, "the schedule has no phases"),
            LoadError::InvalidRate { phase } => {
                write!(f, "phase {phase:?} needs a finite rate of 0 or more")
            }
            LoadError::NoConnections => write!(f, "at least one connection is needed"),
            LoadError::ZeroTimeout => write!(f, "the timeout must be above zero"),
            LoadError::InvalidAuthority(authority) => {
                write!(f, "{authority:?} is not a valid authority")
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// Failed requests by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Errors {
    /// The TCP connect or the protocol handshake failed.
    pub connect: u64,
    /// No complete response within the timeout of the due time.
    pub timeout: u64,
    /// The connection failed while sending or reading.
    pub other: u64,
}

impl Errors {
    /// Every failed request.
    pub fn total(&self) -> u64 {
        self.connect + self.timeout + self.other
    }

    fn add(&mut self, other: &Errors) {
        self.connect += other.connect;
        self.timeout += other.timeout;
        self.other += other.other;
    }
}

/// What one phase saw.
#[derive(Clone, Debug)]
pub struct PhaseReport {
    /// The phase's name.
    pub name: String,
    /// The scheduled rate, requests per second.
    pub rate: f64,
    /// The phase's duration.
    pub duration: Duration,
    /// Whether it was warm-up.
    pub warmup: bool,
    /// Requests the schedule sent.
    pub scheduled: u64,
    /// Requests answered with a complete response.
    pub completed: u64,
    /// Completed responses by status class: 1xx to 5xx.
    pub status_classes: [u64; 5],
    /// Requests that failed.
    pub errors: Errors,
    /// Response body bytes received.
    pub body_bytes: u64,
    /// Nanoseconds from each request's due time to the end of its
    /// response: the latency corrected for coordinated omission.
    pub response_time: Histogram,
    /// Nanoseconds from the moment each request was written to the end of
    /// its response, the latency a closed-loop tool reports.
    pub service_time: Histogram,
}

impl PhaseReport {
    fn new(phase: &Phase) -> Self {
        PhaseReport {
            name: phase.name.clone(),
            rate: phase.rate,
            duration: phase.duration,
            warmup: phase.warmup,
            scheduled: phase.requests(),
            completed: 0,
            status_classes: [0; 5],
            errors: Errors::default(),
            body_bytes: 0,
            response_time: Histogram::default(),
            service_time: Histogram::default(),
        }
    }

    /// Completed responses per second of the phase.
    pub fn throughput(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs > 0.0 {
            self.completed as f64 / secs
        } else {
            0.0
        }
    }

    fn record(&mut self, outcome: Outcome) {
        match outcome.result {
            Ok(Answer {
                status,
                body_bytes,
                sent,
            }) => {
                self.completed += 1;
                let class = (status / 100).clamp(1, 5) as usize - 1;
                self.status_classes[class] += 1;
                self.body_bytes += body_bytes;
                self.response_time
                    .record_duration(outcome.done - outcome.due);
                self.service_time.record_duration(outcome.done - sent);
            }
            Err(Failure::Connect) => self.errors.connect += 1,
            Err(Failure::Timeout) => self.errors.timeout += 1,
            Err(Failure::Other) => self.errors.other += 1,
        }
    }

    fn absorb(&mut self, other: &PhaseReport) {
        self.duration += other.duration;
        self.scheduled += other.scheduled;
        self.completed += other.completed;
        for (mine, theirs) in self.status_classes.iter_mut().zip(other.status_classes) {
            *mine += theirs;
        }
        self.errors.add(&other.errors);
        self.body_bytes += other.body_bytes;
        self.response_time.merge(&other.response_time);
        self.service_time.merge(&other.service_time);
    }
}

/// What a run saw, phase by phase.
#[derive(Clone, Debug)]
pub struct RunReport {
    /// The HTTP version used.
    pub protocol: Protocol,
    /// The connections opened.
    pub connections: usize,
    /// One report per phase of the schedule, in order.
    pub phases: Vec<PhaseReport>,
    /// From the start of the first phase to the last response.
    pub elapsed: Duration,
}

impl RunReport {
    /// Every phase but warm-up merged into one, named `measured`, with
    /// the mean scheduled rate over the measured time.
    pub fn measured(&self) -> PhaseReport {
        let mut total = PhaseReport::new(&Phase::new("measured", 0.0, Duration::ZERO));
        for phase in self.phases.iter().filter(|p| !p.warmup) {
            total.absorb(phase);
        }
        let secs = total.duration.as_secs_f64();
        if secs > 0.0 {
            total.rate = total.scheduled as f64 / secs;
        }
        total
    }
}

/// Runs the schedule against the target and reports what each phase saw.
/// It returns once every request has been answered, has failed or has
/// timed out.
pub async fn run(
    config: LoadConfig,
    source: Arc<dyn RequestSource>,
) -> Result<RunReport, LoadError> {
    let target = Target::new(&config, source)?;
    let (job_tx, job_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let jobs = Arc::new(Mutex::new(job_rx));
    for _ in 0..config.connections {
        tokio::spawn(worker(
            Arc::clone(&target),
            Arc::clone(&jobs),
            out_tx.clone(),
        ));
    }
    drop(out_tx);

    let mut phases: Vec<PhaseReport> = config
        .schedule
        .phases
        .iter()
        .map(PhaseReport::new)
        .collect();
    let start = Instant::now();
    tokio::spawn(dispatch(config.schedule.phases, start, job_tx));
    while let Some(outcome) = out_rx.recv().await {
        let phase = outcome.phase;
        phases[phase].record(outcome);
    }
    Ok(RunReport {
        protocol: config.protocol,
        connections: config.connections,
        phases,
        elapsed: start.elapsed(),
    })
}

/// The validated config every worker shares.
struct Target {
    addr: SocketAddr,
    authority: Authority,
    host: HeaderValue,
    protocol: Protocol,
    timeout: Duration,
    source: Arc<dyn RequestSource>,
}

impl Target {
    fn new(config: &LoadConfig, source: Arc<dyn RequestSource>) -> Result<Arc<Self>, LoadError> {
        if config.schedule.phases.is_empty() {
            return Err(LoadError::NoPhases);
        }
        if let Some(phase) = config
            .schedule
            .phases
            .iter()
            .find(|p| !p.rate.is_finite() || p.rate < 0.0)
        {
            return Err(LoadError::InvalidRate {
                phase: phase.name.clone(),
            });
        }
        if config.connections == 0 {
            return Err(LoadError::NoConnections);
        }
        if config.timeout.is_zero() {
            return Err(LoadError::ZeroTimeout);
        }
        let invalid = || LoadError::InvalidAuthority(config.authority.clone());
        let authority: Authority = config.authority.parse().map_err(|_| invalid())?;
        let host = HeaderValue::from_str(authority.as_str()).map_err(|_| invalid())?;
        Ok(Arc::new(Target {
            addr: config.addr,
            authority,
            host,
            protocol: config.protocol,
            timeout: config.timeout,
            source,
        }))
    }

    /// The source's request numbered `seq`, addressed for the protocol.
    fn request(&self, seq: u64) -> Request<Full<Bytes>> {
        let mut request = self.source.request(seq);
        let path = request
            .uri()
            .path_and_query()
            .cloned()
            .unwrap_or_else(|| PathAndQuery::from_static("/"));
        match self.protocol {
            Protocol::Http1 => {
                *request.uri_mut() = Uri::from(path);
                *request.version_mut() = Version::HTTP_11;
                if !request.headers().contains_key(header::HOST) {
                    request
                        .headers_mut()
                        .insert(header::HOST, self.host.clone());
                }
            }
            Protocol::Http2 => {
                let mut parts = http::uri::Parts::default();
                parts.scheme = Some(Scheme::HTTP);
                parts.authority = Some(self.authority.clone());
                parts.path_and_query = Some(path);
                // Scheme, authority and path are each valid, so the URI is.
                *request.uri_mut() = Uri::from_parts(parts).expect("a valid absolute URI");
                *request.version_mut() = Version::HTTP_2;
                request.headers_mut().remove(header::HOST);
            }
        }
        request
    }

    async fn connect(&self) -> Result<Conn, Failure> {
        let stream = TcpStream::connect(self.addr)
            .await
            .map_err(|_| Failure::Connect)?;
        // Small requests are written whole; Nagle would only delay them.
        let _ = stream.set_nodelay(true);
        let io = TokioIo::new(stream);
        match self.protocol {
            Protocol::Http1 => {
                let (sender, connection) =
                    http1::handshake(io).await.map_err(|_| Failure::Connect)?;
                tokio::spawn(connection);
                Ok(Conn::Http1(sender))
            }
            Protocol::Http2 => {
                let (sender, connection) = http2::handshake(TokioExecutor::new(), io)
                    .await
                    .map_err(|_| Failure::Connect)?;
                tokio::spawn(connection);
                Ok(Conn::Http2(sender))
            }
        }
    }
}

enum Conn {
    Http1(http1::SendRequest<Full<Bytes>>),
    Http2(http2::SendRequest<Full<Bytes>>),
}

impl Conn {
    fn is_closed(&self) -> bool {
        match self {
            Conn::Http1(sender) => sender.is_closed(),
            Conn::Http2(sender) => sender.is_closed(),
        }
    }
}

/// A request due at a moment.
struct Job {
    seq: u64,
    phase: usize,
    due: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Connect,
    Timeout,
    Other,
}

struct Answer {
    status: u16,
    body_bytes: u64,
    sent: Instant,
}

struct Outcome {
    phase: usize,
    due: Instant,
    done: Instant,
    result: Result<Answer, Failure>,
}

/// Hands each request to the workers at its due time, then closes the
/// queue.
async fn dispatch(phases: Vec<Phase>, start: Instant, jobs: mpsc::UnboundedSender<Job>) {
    let mut phase_start = start;
    let mut seq = 0;
    for (index, phase) in phases.iter().enumerate() {
        for k in 0..phase.requests() {
            let due = phase_start + phase.offset(k);
            sleep_until(due).await;
            if jobs
                .send(Job {
                    seq,
                    phase: index,
                    due,
                })
                .is_err()
            {
                return;
            }
            seq += 1;
        }
        phase_start += phase.duration;
    }
}

/// Owns one connection and serves queued requests on it until the queue
/// closes.
async fn worker(
    target: Arc<Target>,
    jobs: Arc<Mutex<mpsc::UnboundedReceiver<Job>>>,
    out: mpsc::UnboundedSender<Outcome>,
) {
    let mut conn: Option<Conn> = None;
    loop {
        let Some(job) = jobs.lock().await.recv().await else {
            return;
        };
        let deadline = job.due + target.timeout;
        let report = |result| {
            let _ = out.send(Outcome {
                phase: job.phase,
                due: job.due,
                done: Instant::now(),
                result,
            });
        };
        if Instant::now() >= deadline {
            // It waited out its whole timeout in the queue.
            report(Err(Failure::Timeout));
            continue;
        }
        match ready(&target, &mut conn, deadline).await {
            Err(failure) => report(Err(failure)),
            Ok(()) => match conn.as_mut() {
                Some(Conn::Http1(sender)) => {
                    let sent = Instant::now();
                    let result =
                        exchange(sender.send_request(target.request(job.seq)), sent, deadline)
                            .await;
                    if result.is_err() {
                        // A response may still be on its way; never read it
                        // as the next request's.
                        conn = None;
                    }
                    report(result);
                }
                Some(Conn::Http2(sender)) => {
                    let mut sender = sender.clone();
                    let request = target.request(job.seq);
                    let out = out.clone();
                    tokio::spawn(async move {
                        let result = match timeout_at(deadline, sender.ready()).await {
                            Ok(Ok(())) => {
                                exchange(sender.send_request(request), Instant::now(), deadline)
                                    .await
                            }
                            Ok(Err(_)) => Err(Failure::Other),
                            Err(_) => Err(Failure::Timeout),
                        };
                        let _ = out.send(Outcome {
                            phase: job.phase,
                            due: job.due,
                            done: Instant::now(),
                            result,
                        });
                    });
                }
                // `ready` leaves a connection in place when it succeeds.
                None => unreachable!("ready returned without a connection"),
            },
        }
    }
}

/// Makes sure `conn` holds a connection that can take a request now,
/// opening a new one when there is none or the old one has closed.
async fn ready(target: &Target, conn: &mut Option<Conn>, deadline: Instant) -> Result<(), Failure> {
    // An HTTP/1.1 connection the server has just closed can look open
    // until it is asked; one fresh connection gets a second chance.
    for _ in 0..2 {
        if conn.as_ref().is_none_or(Conn::is_closed) {
            *conn = None;
            *conn = Some(
                timeout_at(deadline, target.connect())
                    .await
                    .map_err(|_| Failure::Timeout)??,
            );
        }
        match conn.as_mut() {
            Some(Conn::Http1(sender)) => match timeout_at(deadline, sender.ready()).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(_)) => *conn = None,
                Err(_) => return Err(Failure::Timeout),
            },
            // HTTP/2 waits for stream capacity per request instead.
            _ => return Ok(()),
        }
    }
    Err(Failure::Other)
}

/// Waits for the response and reads its body to the end, by the deadline.
async fn exchange<F>(response: F, sent: Instant, deadline: Instant) -> Result<Answer, Failure>
where
    F: Future<Output = hyper::Result<Response<Incoming>>>,
{
    let read = async {
        let response = response.await?;
        let status = response.status().as_u16();
        let mut body = response.into_body();
        let mut body_bytes = 0u64;
        while let Some(frame) = body.frame().await {
            if let Some(data) = frame?.data_ref() {
                body_bytes += data.len() as u64;
            }
        }
        Ok::<_, hyper::Error>(Answer {
            status,
            body_bytes,
            sent,
        })
    };
    match timeout_at(deadline, read).await {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(_)) => Err(Failure::Other),
        Err(_) => Err(Failure::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hyper::service::service_fn;
    use tokio::net::TcpListener;

    /// What the test server does and has seen.
    #[derive(Default)]
    struct ServerState {
        requests: AtomicU64,
        http2: AtomicU64,
        connections: AtomicU64,
        /// Every request that arrives before this waits for it.
        stall_until: StdMutex<Option<Instant>>,
    }

    impl ServerState {
        fn stall(&self, length: Duration) {
            *self.stall_until.lock().unwrap() = Some(Instant::now() + length);
        }
    }

    /// Serves `/` with a 64-byte body, `/status/N` with status N and
    /// `/sleep/MS` after MS milliseconds, until the test ends. `keep_alive` false closes each
    /// HTTP/1.1 connection after one response.
    async fn server(protocol: Protocol, keep_alive: bool) -> (SocketAddr, Arc<ServerState>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(ServerState::default());
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                shared.connections.fetch_add(1, Ordering::Relaxed);
                let state = Arc::clone(&shared);
                let service = service_fn(move |request: Request<Incoming>| {
                    let state = Arc::clone(&state);
                    async move {
                        state.requests.fetch_add(1, Ordering::Relaxed);
                        if request.version() == Version::HTTP_2 {
                            state.http2.fetch_add(1, Ordering::Relaxed);
                        }
                        let until = *state.stall_until.lock().unwrap();
                        if let Some(until) = until {
                            sleep_until(until).await;
                        }
                        let path = request.uri().path().to_owned();
                        let mut response =
                            Response::new(Full::new(Bytes::from_static(&[b'x'; 64])));
                        if let Some(code) = path.strip_prefix("/status/") {
                            *response.status_mut() =
                                code.parse::<u16>().unwrap().try_into().unwrap();
                        } else if let Some(ms) = path.strip_prefix("/sleep/") {
                            tokio::time::sleep(Duration::from_millis(ms.parse().unwrap())).await;
                        }
                        Ok::<_, Infallible>(response)
                    }
                });
                let io = TokioIo::new(stream);
                tokio::spawn(async move {
                    match protocol {
                        Protocol::Http1 => {
                            let _ = hyper::server::conn::http1::Builder::new()
                                .keep_alive(keep_alive)
                                .serve_connection(io, service)
                                .await;
                        }
                        Protocol::Http2 => {
                            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                                .serve_connection(io, service)
                                .await;
                        }
                    }
                });
            }
        });
        (addr, state)
    }

    fn get(path: &str) -> Arc<dyn RequestSource> {
        Arc::new(Get(path.to_owned()))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn holds_a_constant_rate_over_http1() {
        let (addr, state) = server(Protocol::Http1, true).await;
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http1,
            Schedule::constant(200.0, Duration::from_secs(1)),
        );
        config.connections = 4;
        let report = run(config, get("/")).await.unwrap();
        assert_eq!(report.phases.len(), 1);
        let phase = &report.phases[0];
        assert_eq!(phase.name, "constant");
        assert_eq!(phase.scheduled, 200);
        assert_eq!(phase.completed, 200, "{:?}", phase.errors);
        assert_eq!(phase.errors.total(), 0);
        assert_eq!(phase.status_classes, [0, 200, 0, 0, 0]);
        assert_eq!(phase.body_bytes, 200 * 64);
        assert_eq!(phase.response_time.len(), 200);
        assert_eq!(phase.service_time.len(), 200);
        assert_eq!(phase.throughput(), 200.0);
        // The last request is due at 995 ms: the schedule set the pace.
        assert!(
            report.elapsed >= Duration::from_millis(995),
            "{:?}",
            report.elapsed
        );
        assert_eq!(state.requests.load(Ordering::Relaxed), 200);
        assert!(state.connections.load(Ordering::Relaxed) <= 4);
        assert_eq!(state.http2.load(Ordering::Relaxed), 0);
        assert_eq!(report.protocol, Protocol::Http1);
        assert_eq!(report.connections, 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiplexes_over_one_http2_connection() {
        let (addr, state) = server(Protocol::Http2, true).await;
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http2,
            Schedule::constant(400.0, Duration::from_millis(500)),
        );
        config.connections = 1;
        config.authority = "example.test".to_owned();
        let report = run(config, get("/status/204")).await.unwrap();
        let phase = &report.phases[0];
        assert_eq!(phase.completed, 200, "{:?}", phase.errors);
        assert_eq!(phase.status_classes, [0, 200, 0, 0, 0]);
        assert_eq!(state.http2.load(Ordering::Relaxed), 200);
        assert_eq!(state.connections.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http2_requests_overlap_on_one_connection() {
        // 100 ms responses at 100 per second on one connection: only
        // concurrent streams keep the response time near 100 ms.
        let (addr, _) = server(Protocol::Http2, true).await;
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http2,
            Schedule::constant(100.0, Duration::from_millis(500)),
        );
        config.connections = 1;
        let report = run(config, get("/sleep/100")).await.unwrap();
        let phase = &report.phases[0];
        assert_eq!(phase.completed, 50, "{:?}", phase.errors);
        assert!(
            phase.response_time.max() < 400_000_000,
            "{}",
            phase.response_time.max()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stepped_schedule_reports_each_step() {
        let (addr, state) = server(Protocol::Http1, true).await;
        let schedule = Schedule::stepped(100.0, 200.0, 2, Duration::from_millis(500))
            .with_warmup(50.0, Duration::from_millis(200));
        assert_eq!(schedule.duration(), Duration::from_millis(1200));
        let names: Vec<_> = schedule.phases().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["warmup", "step-1", "step-2"]);
        let mut config = LoadConfig::new(addr, Protocol::Http1, schedule);
        config.connections = 4;
        let report = run(config, get("/status/404")).await.unwrap();
        let counts: Vec<_> = report
            .phases
            .iter()
            .map(|p| (p.scheduled, p.completed))
            .collect();
        assert_eq!(counts, [(10, 10), (50, 50), (150, 150)]);
        assert!(report.phases[0].warmup);
        for phase in &report.phases {
            assert_eq!(phase.response_time.len(), phase.completed);
            assert_eq!(phase.status_classes[3], phase.completed);
        }
        let measured = report.measured();
        assert_eq!(measured.name, "measured");
        assert_eq!(measured.scheduled, 200);
        assert_eq!(measured.completed, 200);
        assert_eq!(measured.duration, Duration::from_secs(1));
        assert_eq!(measured.rate, 200.0);
        assert_eq!(measured.response_time.len(), 200);
        assert_eq!(state.requests.load(Ordering::Relaxed), 210);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stall_shows_in_response_time_but_hides_from_service_time() {
        let (addr, state) = server(Protocol::Http1, true).await;
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http1,
            Schedule::constant(500.0, Duration::from_secs(1)),
        );
        config.connections = 2;
        let stall = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            stall.stall(Duration::from_millis(100));
        });
        let report = run(config, get("/")).await.unwrap();
        let phase = &report.phases[0];
        assert_eq!(phase.completed, 500, "{:?}", phase.errors);
        // About 50 of the 500 requests were due during the stall, waiting
        // up to 100 ms each: well over 1% of them took 80 ms or more.
        let response_p99 = phase.response_time.value_at_quantile(0.99);
        assert!(response_p99 >= 80_000_000, "response p99 {response_p99}");
        assert!(phase.response_time.max() >= 95_000_000);
        // Only the two requests on the wire saw it as service time.
        let service_p99 = phase.service_time.value_at_quantile(0.99);
        assert!(service_p99 < response_p99, "service p99 {service_p99}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeouts_count_requests_that_waited_too_long() {
        let (addr, _) = server(Protocol::Http1, true).await;
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http1,
            Schedule::constant(50.0, Duration::from_millis(200)),
        );
        config.connections = 1;
        config.timeout = Duration::from_millis(100);
        let report = run(config, get("/sleep/300")).await.unwrap();
        let phase = &report.phases[0];
        assert_eq!(phase.scheduled, 10);
        assert_eq!(phase.completed, 0);
        assert_eq!(phase.errors.timeout, 10);
        assert!(phase.response_time.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http2_streams_time_out_on_their_own() {
        let (addr, _) = server(Protocol::Http2, true).await;
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http2,
            Schedule::constant(50.0, Duration::from_millis(200)),
        );
        config.connections = 1;
        config.timeout = Duration::from_millis(100);
        let report = run(config, get("/sleep/300")).await.unwrap();
        assert_eq!(report.phases[0].errors.timeout, 10);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnects_when_the_server_closes_each_connection() {
        let (addr, state) = server(Protocol::Http1, false).await;
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http1,
            Schedule::constant(100.0, Duration::from_millis(300)),
        );
        config.connections = 1;
        let report = run(config, get("/")).await.unwrap();
        let phase = &report.phases[0];
        assert_eq!(phase.completed, 30, "{:?}", phase.errors);
        assert_eq!(state.connections.load(Ordering::Relaxed), 30);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn counts_connect_failures() {
        // A port that was free a moment ago refuses connections.
        let addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        for protocol in [Protocol::Http1, Protocol::Http2] {
            let config = LoadConfig::new(
                addr,
                protocol,
                Schedule::constant(100.0, Duration::from_millis(100)),
            );
            let report = run(config, get("/")).await.unwrap();
            assert_eq!(
                report.phases[0].errors,
                Errors {
                    connect: 10,
                    timeout: 0,
                    other: 0
                }
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_http1_only_server_fails_http2_requests() {
        // HTTP/2 prior knowledge against an HTTP/1.1-only server.
        let (addr, _) = server(Protocol::Http1, true).await;
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http2,
            Schedule::constant(20.0, Duration::from_millis(100)),
        );
        config.connections = 1;
        config.timeout = Duration::from_millis(500);
        let report = run(config, get("/")).await.unwrap();
        let phase = &report.phases[0];
        assert_eq!(phase.completed, 0);
        assert_eq!(phase.errors.total(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_connection_counts_as_other() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Read the request, then hang up without answering.
            while let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
            }
        });
        let mut config = LoadConfig::new(
            addr,
            Protocol::Http1,
            Schedule::constant(20.0, Duration::from_millis(100)),
        );
        config.connections = 1;
        let report = run(config, get("/")).await.unwrap();
        assert_eq!(
            report.phases[0].errors,
            Errors {
                connect: 0,
                timeout: 0,
                other: 2
            }
        );
    }

    #[tokio::test]
    async fn a_pause_sends_nothing() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let config = LoadConfig::new(
            addr,
            Protocol::Http1,
            Schedule::new(vec![Phase::new("idle", 0.0, Duration::from_millis(10))]),
        );
        let report = run(config, get("/")).await.unwrap();
        assert_eq!(report.phases[0].scheduled, 0);
        assert_eq!(report.phases[0].throughput(), 0.0);
        let measured = report.measured();
        assert_eq!(measured.rate, 0.0);
        let mut empty = report.clone();
        empty.phases.clear();
        assert_eq!(empty.measured().throughput(), 0.0);
    }

    #[tokio::test]
    async fn rejects_configs_it_cannot_run() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let good = || {
            LoadConfig::new(
                addr,
                Protocol::Http1,
                Schedule::constant(1.0, Duration::from_secs(1)),
            )
        };
        let cases = [
            (
                LoadConfig {
                    schedule: Schedule::new(vec![]),
                    ..good()
                },
                LoadError::NoPhases,
            ),
            (
                LoadConfig {
                    schedule: Schedule::constant(f64::NAN, Duration::from_secs(1)),
                    ..good()
                },
                LoadError::InvalidRate {
                    phase: "constant".into(),
                },
            ),
            (
                LoadConfig {
                    schedule: Schedule::constant(-1.0, Duration::from_secs(1)),
                    ..good()
                },
                LoadError::InvalidRate {
                    phase: "constant".into(),
                },
            ),
            (
                LoadConfig {
                    connections: 0,
                    ..good()
                },
                LoadError::NoConnections,
            ),
            (
                LoadConfig {
                    timeout: Duration::ZERO,
                    ..good()
                },
                LoadError::ZeroTimeout,
            ),
            (
                LoadConfig {
                    authority: "bad host".into(),
                    ..good()
                },
                LoadError::InvalidAuthority("bad host".into()),
            ),
        ];
        for (config, expected) in cases {
            let err = run(config, get("/")).await.unwrap_err();
            assert!(!err.to_string().is_empty());
            assert_eq!(err, expected);
        }
    }

    #[test]
    fn requests_are_addressed_for_the_protocol() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let source: Arc<dyn RequestSource> = Arc::new(|seq: u64| {
            let mut request = Request::new(Full::new(Bytes::new()));
            *request.uri_mut() = format!("http://ignored/k/{seq}?v=1").parse().unwrap();
            if seq == 1 {
                request
                    .headers_mut()
                    .insert(header::HOST, HeaderValue::from_static("custom"));
            }
            request
        });
        let config = LoadConfig::new(
            addr,
            Protocol::Http1,
            Schedule::constant(1.0, Duration::from_secs(1)),
        );
        let h1 = Target::new(&config, Arc::clone(&source)).unwrap();
        let request = h1.request(0);
        assert_eq!(request.uri(), "/k/0?v=1");
        assert_eq!(request.version(), Version::HTTP_11);
        assert_eq!(request.headers()[header::HOST], "127.0.0.1:8080");
        assert_eq!(h1.request(1).headers()[header::HOST], "custom");

        let config = LoadConfig::new(
            addr,
            Protocol::Http2,
            Schedule::constant(1.0, Duration::from_secs(1)),
        );
        let h2 = Target::new(&config, source).unwrap();
        let request = h2.request(1);
        assert_eq!(request.uri(), "http://127.0.0.1:8080/k/1?v=1");
        assert_eq!(request.version(), Version::HTTP_2);
        assert!(!request.headers().contains_key(header::HOST));

        // A source with no path asks for `/`; a bad one leaves the default.
        let none = Target::new(&config, Arc::new(Get("not a path".into()))).unwrap();
        assert_eq!(none.request(0).uri(), "http://127.0.0.1:8080/");
    }

    #[test]
    fn phases_count_requests_due_before_their_end() {
        assert_eq!(
            Phase::new("a", 200.0, Duration::from_secs(1)).requests(),
            200
        );
        assert_eq!(
            Phase::new("a", 300.0, Duration::from_millis(500)).requests(),
            150
        );
        assert_eq!(Phase::new("a", 0.1, Duration::from_secs(30)).requests(), 3);
        assert_eq!(
            Phase::new("a", 3.0, Duration::from_millis(500)).requests(),
            2
        );
        assert_eq!(Phase::new("a", 0.0, Duration::from_secs(5)).requests(), 0);
        let phase = Phase::new("a", 4.0, Duration::from_secs(1));
        assert_eq!(phase.offset(3), Duration::from_millis(750));
        assert!(!phase.warmup);
        assert!(phase.warmup().warmup);
    }
}
