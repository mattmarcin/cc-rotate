//! The loopback relay of relay mode: a reverse proxy from `127.0.0.1:<port>` to Anthropic's API
//! that puts the active pool account's login on model requests.
//!
//! The design is in `docs/specs/2026-11-10-relay-mode.md`, sections 5 and 13.
//! The rules for which requests pass and which headers go on are in `ccr_core::relay_proto`.

pub mod events;
pub mod login;

use std::borrow::Cow;
use std::convert::Infallible;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use ccr_core::relay::RelayConfig;
use ccr_core::relay_proto::{self, Route};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, Version, header};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use zeroize::Zeroizing;

pub use events::{StoreEvents, StoreSwaps, Swaps};
pub use login::{Renewals, StoreLogins, StoreRenewals};

/// The only address the relay sends requests to in a release build.
const ANTHROPIC: &str = "https://api.anthropic.com";

/// The header Claude Code names its session in.
const SESSION_HEADER: &str = "x-claude-code-session-id";

/// How long a leaving relay waits for its connections to close: a request still running then is
/// cut off.
/// Above Claude Code's own request timeout (600 s), so a request it still waits for is never cut.
const CLOSE_WITHIN: Duration = Duration::from_secs(660);

/// How often a request held after a 401 reads the stored login again (spec 9).
const REFRESH_EVERY: Duration = Duration::from_millis(500);

/// How the relay behaves, apart from where it listens.
#[derive(Clone)]
pub struct Options {
    /// How long a model request refused as not signed in waits for a renewed login before the
    /// relay swaps away from its account (spec 9).
    pub login_hold: Duration,
    /// How often the relay checks whether its program file was replaced and whether it has been
    /// idle long enough to leave (spec 10).
    pub check_every: Duration,
    /// How long without a request before the relay leaves for good once relay mode is off.
    pub idle_exit: Duration,
    /// The program file whose replacement is an upgrade: the running executable when `None`.
    pub program: Option<PathBuf>,
    /// `program`'s fingerprint when the process started, so an upgrade while it waited to serve
    /// (for the relay lock) still counts; taken when serving starts when `None`.
    pub program_at_start: Option<Fingerprint>,
    /// Says whether relay mode is on, and removes the relay's service when it leaves for good.
    pub lifecycle: Arc<dyn Lifecycle>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            login_hold: Duration::from_secs(30),
            check_every: Duration::from_secs(60),
            idle_exit: Duration::from_secs(3600),
            program: None,
            program_at_start: None,
            lifecycle: Arc::new(Stays),
        }
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("login_hold", &self.login_hold)
            .field("check_every", &self.check_every)
            .field("idle_exit", &self.idle_exit)
            .field("program", &self.program)
            .field("program_at_start", &self.program_at_start)
            .finish_non_exhaustive()
    }
}

/// What decides whether the relay may leave for good (spec 10).
/// Every method is called on a thread that may block.
pub trait Lifecycle: Send + Sync {
    /// Whether relay mode is on. An error reading it counts as on: the relay stays.
    fn mode_on(&self) -> bool {
        true
    }

    /// Relay mode is off and no request came for `Options::idle_exit`: remove the relay's
    /// service and `relay.json`, so nothing starts the relay again.
    fn retire(&self) -> Result<()> {
        Ok(())
    }
}

/// A relay that never leaves for being idle.
pub struct Stays;

impl Lifecycle for Stays {}

/// Which login a model request goes out with.
#[derive(Clone)]
pub enum Choice {
    /// The session's own login: the active account is the one the profile is signed in to.
    PassThrough { account: i64 },
    /// The active account's stored access token.
    Token {
        account: i64,
        token: Zeroizing<String>,
    },
}

impl Choice {
    /// The account the request goes out on.
    pub fn account(&self) -> i64 {
        match self {
            Choice::PassThrough { account } | Choice::Token { account, .. } => *account,
        }
    }
}

impl std::fmt::Debug for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Choice::PassThrough { account } => write!(f, "PassThrough {{ account: {account} }}"),
            Choice::Token { account, .. } => write!(f, "Token {{ account: {account}, .. }}"),
        }
    }
}

/// Where the relay gets the login for a model request.
/// Every method is called on a thread that may block.
pub trait Logins: Send + Sync {
    /// The login for a model request, called once per request.
    fn for_model(&self) -> Result<Choice>;

    /// Start a renewal of `account`'s stored login in the background, unless one is running,
    /// and return at once (spec 9).
    /// Never for the account whose login is the profile's: Claude Code renews that one itself.
    /// Never with relay mode off: the relay then only forwards.
    fn renew(&self, account: i64);

    /// `account`'s stored login as it is now: a renewal may have replaced it.
    fn refresh(&self, account: i64) -> Result<Choice>;
}

/// What the relay reports as it works. Every method does nothing unless implemented.
pub trait RelayEvents: Send + Sync {
    /// A model request that went out with a known account got its response's status and headers.
    /// With relay mode off nothing is recorded: the status line records the readings then.
    /// Called on a thread that may block, before the response goes back to the session.
    fn response(
        &self,
        _account: i64,
        _session: Option<&str>,
        _model: Option<&str>,
        _status: u16,
        _headers: &[(String, String)],
    ) {
    }

    /// One line for the relay's log.
    /// It never holds a token, a body, a query or a request header's value.
    fn log(&self, _line: &str) {}

    /// Anthropic refused a model request sent with `account`'s login at a usage limit, before
    /// anything of the response went back to the session (spec 8).
    /// Swaps away from `account` if it is still the active one, waiting for a swap already
    /// running, and says which account is active now.
    /// With relay mode off it does nothing and says `Swapped::No`: the refusal goes back as it is.
    /// Called on a thread that may block.
    fn refused_at_limit(&self, _account: i64) -> Swapped {
        Swapped::No
    }

    /// Anthropic refused a model request sent with `account`'s stored login as not signed in, and
    /// no renewed login came within the hold (spec 9).
    /// Holds the account back until a check settles its login, then swaps away from it as
    /// `refused_at_limit` does (reason `unauthorized`), and says which account is active now.
    /// With relay mode off it does nothing and says `Swapped::No`.
    /// Called on a thread that may block.
    fn dead_login(&self, _account: i64) -> Swapped {
        Swapped::No
    }

    /// A request refused on `from` is being sent again on `to`.
    /// Called on a thread that may block.
    fn resent(&self, _from: i64, _to: i64) {}

    /// Anthropic could not be reached; `kind` names what failed, never a value.
    /// Called on a thread that may block.
    fn unreachable(&self, _kind: &str) {}
}

/// What became of a swap away from an account refused at a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swapped {
    /// Another account is active now.
    To(i64),
    /// The refused account is still the active one, or none is.
    No,
}

/// Events that only write the log lines to a file: `relay.log`, rotated at 1 MB like cc-rotate's
/// own log, readable by the owner alone.
///
/// A thread of its own does the writing, so logging never blocks the threads serving requests.
pub struct FileLog {
    tx: mpsc::Sender<String>,
}

impl FileLog {
    pub fn new(path: impl Into<PathBuf>) -> FileLog {
        let path = path.into();
        let (tx, rx) = mpsc::channel::<String>();
        // Without the thread, lines are dropped: the relay keeps serving.
        let _ = std::thread::Builder::new()
            .name("relay-log".into())
            .spawn(move || {
                for line in rx {
                    write_line(&path, &line);
                }
            });
        FileLog { tx }
    }
}

impl RelayEvents for FileLog {
    fn log(&self, line: &str) {
        let _ = self.tx.send(format!("{} {line}", ccr_core::time::now_ms()));
    }
}

/// Write one line to the relay's log at `path` now, as `FileLog` does, for when no `FileLog` runs
/// yet (the relay could not start).
pub fn log_now(path: &Path, line: &str) {
    write_line(path, &format!("{} {line}", ccr_core::time::now_ms()));
}

fn write_line(path: &Path, line: &str) {
    if let Some(dir) = path.parent() {
        let _ = ccr_core::secret::create_private_dir(dir);
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 1_000_000) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(path, old);
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(path) {
        let _ = writeln!(file, "{line}");
    }
}

type Connector = HttpsConnector<HttpConnector>;

/// Where requests go: `https://api.anthropic.com`, with TLS verified against the system's roots.
pub struct Upstream {
    /// Scheme and authority, with no path: the request's path is put after it.
    origin: String,
    scheme: http::uri::Scheme,
    authority: http::uri::Authority,
    client: Client<Connector, Full<Bytes>>,
}

impl Upstream {
    /// Anthropic's API, over HTTPS only.
    pub fn anthropic() -> Result<Upstream> {
        Upstream::build(ANTHROPIC.parse()?, true)
    }

    /// A local server instead of Anthropic's API, over plain HTTP or HTTPS. Only in test builds.
    #[cfg(feature = "test-upstream")]
    pub fn for_test(base: Uri) -> Result<Upstream> {
        Upstream::build(base, false)
    }

    fn build(base: Uri, https_only: bool) -> Result<Upstream> {
        let scheme = base
            .scheme()
            .cloned()
            .context("the upstream has no scheme")?;
        let authority = base
            .authority()
            .cloned()
            .context("the upstream has no host")?;
        let builder = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_platform_verifier(rustls::crypto::ring::default_provider())
            .context("setting up TLS with the system's roots")?;
        let builder = if https_only {
            builder.https_only()
        } else {
            builder.https_or_http()
        };
        let connector = builder.enable_http1().enable_http2().build();
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(connector);
        Ok(Upstream {
            origin: format!("{scheme}://{authority}"),
            scheme,
            authority,
            client,
        })
    }

    /// The address for a path from `relay_proto::route`, which always starts with `/`.
    /// The path is put after the fixed origin; it can never change the scheme, host or port.
    fn uri(&self, path: &str) -> Option<Uri> {
        if !path.starts_with('/') {
            return None;
        }
        let uri: Uri = format!("{}{path}", self.origin).parse().ok()?;
        (uri.scheme() == Some(&self.scheme) && uri.authority() == Some(&self.authority))
            .then_some(uri)
    }
}

struct State {
    cfg: RelayConfig,
    options: Options,
    upstream: Upstream,
    logins: Arc<dyn Logins>,
    events: Arc<dyn RelayEvents>,
    in_flight: Arc<InFlight>,
    /// When the last request with the relay's secret arrived, or the relay started.
    last_request: std::sync::Mutex<tokio::time::Instant>,
}

/// How many requests are in flight: from their arrival until their response's body has gone
/// back to the session, or the session went away.
#[derive(Default)]
struct InFlight {
    count: AtomicUsize,
    zero: tokio::sync::Notify,
}

impl InFlight {
    fn enter(self: &Arc<Self>) -> Entered {
        self.count.fetch_add(1, Ordering::SeqCst);
        Entered(self.clone())
    }

    fn now(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Wait until no request is in flight.
    async fn idle(&self) {
        loop {
            let notified = self.zero.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            if self.now() == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// One request in flight, until dropped.
struct Entered(Arc<InFlight>);

impl Drop for Entered {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.zero.notify_waiters();
        }
    }
}

/// A response body that counts its request in flight until it is done or dropped.
struct Tracked {
    inner: RelayBody,
    _entered: Entered,
}

impl hyper::body::Body for Tracked {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Why the relay leaves (spec 10).
enum Leave {
    /// Its program file was replaced: the service starts the new one.
    Upgrade,
    /// Relay mode is off and no request came for a while.
    Idle,
}

/// What tells a replaced program file from the one the relay started from: its modification
/// time and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint(std::time::SystemTime, u64);

/// The fingerprint of the program file at `path`, `None` when it cannot be read.
pub fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Fingerprint(meta.modified().ok()?, meta.len()))
}

/// Whether the relay should leave now.
async fn check(
    st: &Arc<State>,
    program: Option<&Path>,
    started: Option<Fingerprint>,
) -> Option<Leave> {
    if let (Some(program), Some(started)) = (program, started)
        && fingerprint(program) != Some(started)
    {
        return Some(Leave::Upgrade);
    }
    let quiet = st
        .last_request
        .lock()
        .map(|t| t.elapsed())
        .unwrap_or_default();
    if quiet < st.options.idle_exit || st.in_flight.now() > 0 {
        return None;
    }
    let lifecycle = st.options.lifecycle.clone();
    let on = tokio::task::spawn_blocking(move || lifecycle.mode_on())
        .await
        .unwrap_or(true);
    (!on).then_some(Leave::Idle)
}

type RelayBody = BoxBody<Bytes, hyper::Error>;

/// Serve requests from `listener`, which must be bound to 127.0.0.1, until the relay leaves
/// (spec 10): its program file was replaced, or relay mode is off and no request came for
/// `Options::idle_exit`.
/// After an upgrade it serves on, new connections too, until no request is in flight.
/// Either way it then stops accepting connections, closes the open ones once the request on
/// each is done, and returns; when idle, it then removes its service (`Lifecycle::retire`).
pub async fn serve(
    listener: std::net::TcpListener,
    cfg: RelayConfig,
    options: Options,
    upstream: Upstream,
    logins: Arc<dyn Logins>,
    events: Arc<dyn RelayEvents>,
) -> Result<()> {
    let addr = listener.local_addr()?;
    if addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        bail!(
            "the local proxy listens on 127.0.0.1 only, not {}",
            addr.ip()
        );
    }
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let program = options
        .program
        .clone()
        .or_else(|| std::env::current_exe().ok());
    let started = options
        .program_at_start
        .or_else(|| program.as_deref().and_then(fingerprint));
    let mut ticks = tokio::time::interval(options.check_every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is at once.
    ticks.tick().await;
    let state = Arc::new(State {
        cfg,
        options,
        upstream,
        logins,
        events,
        in_flight: Arc::new(InFlight::default()),
        last_request: std::sync::Mutex::new(tokio::time::Instant::now()),
    });
    // Every connection, so leaving can close each one once its request is done.
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    // Set once the program file was replaced: the relay serves on until nothing is in flight.
    let mut draining = false;
    let leave = loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    // Out of file descriptors, for one: wait for some to close.
                    state.events.log(&format!("accept failed: {e}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = ticks.tick(), if !draining => {
                match check(&state, program.as_deref(), started).await {
                    Some(Leave::Upgrade) => {
                        state.events.log(&format!(
                            "program replaced; leaving once no request is in flight ({} now)",
                            state.in_flight.now()
                        ));
                        draining = true;
                        continue;
                    }
                    Some(Leave::Idle) => break Leave::Idle,
                    None => continue,
                }
            }
            _ = state.in_flight.idle(), if draining => break Leave::Upgrade,
        };
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        let watcher = graceful.watcher();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let state = state.clone();
                async move {
                    let entered = state.in_flight.enter();
                    let resp = handle(req, &state).await;
                    Ok::<_, Infallible>(resp.map(|inner| {
                        Tracked {
                            inner,
                            _entered: entered,
                        }
                        .boxed()
                    }))
                }
            });
            let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            // The timer lets hyper drop a connection whose request headers never finish.
            builder.http1().timer(TokioTimer::new());
            let connection = builder.serve_connection(TokioIo::new(stream), service);
            let _ = watcher.watch(connection).await;
        });
    };
    // No new connections from here on: a session's next one finds the next relay. Kept-alive
    // connections close once the request on them, if any, is done, its last frames sent.
    drop(listener);
    if tokio::time::timeout(CLOSE_WITHIN, graceful.shutdown())
        .await
        .is_err()
    {
        state.events.log(&format!(
            "connections still open after {} s; leaving anyway",
            CLOSE_WITHIN.as_secs()
        ));
    }
    match leave {
        Leave::Upgrade => state.events.log("left for the new program"),
        Leave::Idle => {
            state.events.log(&format!(
                "the app account is off and no request came for {} s; left",
                state.options.idle_exit.as_secs()
            ));
            let lifecycle = state.options.lifecycle.clone();
            match tokio::task::spawn_blocking(move || lifecycle.retire()).await {
                Ok(Ok(())) => state.events.log("removed the local proxy's service"),
                Ok(Err(e)) => state
                    .events
                    .log(&format!("removing the local proxy's service failed: {e:#}")),
                Err(_) => state
                    .events
                    .log("removing the local proxy's service failed"),
            }
        }
    }
    Ok(())
}

/// `serve` on a runtime of its own, for `cc-rotate-ctl relay run`: returns when the relay leaves.
pub fn run_blocking(
    listener: std::net::TcpListener,
    cfg: RelayConfig,
    options: Options,
    upstream: Upstream,
    logins: Arc<dyn Logins>,
    events: Arc<dyn RelayEvents>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the local proxy's runtime")?;
    runtime.block_on(serve(listener, cfg, options, upstream, logins, events))
}

/// This relay's version, as text: the answer to `GET /r/<secret>/__ccr/version` (spec 11).
fn version() -> Response<RelayBody> {
    let body = Full::new(Bytes::from_static(env!("CARGO_PKG_VERSION").as_bytes()));
    let mut resp = Response::new(body.map_err(|n| match n {}).boxed());
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    resp
}

fn bare(status: StatusCode) -> Response<RelayBody> {
    let mut resp = Response::new(Empty::<Bytes>::new().map_err(|n| match n {}).boxed());
    *resp.status_mut() = status;
    resp
}

/// The `Host` the request names: its one `Host` header, or an HTTP/2 request's authority.
/// `None` when there is none, more than one, or the two disagree.
fn host_of<B>(req: &Request<B>) -> Option<&str> {
    let mut hosts = req.headers().get_all(header::HOST).iter();
    let header = match (hosts.next(), hosts.next()) {
        (Some(h), None) => Some(h.to_str().ok()?),
        (None, _) => None,
        (Some(_), Some(_)) => return None,
    };
    let authority = req.uri().authority().map(|a| a.as_str());
    match (header, authority) {
        (Some(h), Some(a)) if h != a => None,
        (Some(h), _) => Some(h),
        (None, Some(a)) if req.version() == Version::HTTP_2 => Some(a),
        _ => None,
    }
}

/// Whether the session signs in with a bearer token, as opposed to an API key.
fn has_bearer(headers: &HeaderMap) -> bool {
    let mut auth = headers.get_all(header::AUTHORIZATION).iter();
    match (auth.next(), auth.next()) {
        (Some(v), None) => v
            .as_bytes()
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"bearer ")),
        _ => false,
    }
}

/// The `model` a request body names, without reading the rest of it into memory twice.
fn model_of(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ModelOnly<'a> {
        #[serde(borrow, default)]
        model: Option<Cow<'a, str>>,
    }
    serde_json::from_slice::<ModelOnly>(body)
        .ok()?
        .model
        .map(Cow::into_owned)
}

/// A header value fit for the log: letters, digits, `-` and `_`, at most 128 of them.
fn loggable(value: Option<&HeaderValue>) -> String {
    let v: String = value
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(128)
        .collect();
    if v.is_empty() { "-".into() } else { v }
}

/// The usage headers of a response, for the log (spec 13): `<name>=<value>` for each
/// `anthropic-ratelimit-unified-*` header, the prefix left out. They are usage numbers, reset
/// times and states, not secrets; any other character is dropped.
fn usage_note(headers: &HeaderMap) -> String {
    fn clean(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '+'))
            .take(64)
            .collect()
    }
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = clean(name.as_str().strip_prefix(USAGE_PREFIX)?);
            let value = clean(value.to_str().unwrap_or(""));
            Some(format!(
                "{name}={}",
                if value.is_empty() { "-" } else { &value }
            ))
        })
        .take(16)
        .collect::<Vec<_>>()
        .join(" ")
}

const USAGE_PREFIX: &str = "anthropic-ratelimit-unified-";

/// The login for a model request, from `Logins` on a thread that may block; the error is a note
/// for the log.
async fn login(st: &State) -> Result<Choice, String> {
    let logins = st.logins.clone();
    match tokio::task::spawn_blocking(move || logins.for_model()).await {
        Ok(Ok(choice)) => Ok(choice),
        Ok(Err(e)) => Err(format!("no login: {e:#}")),
        Err(_) => Err("no login: the lookup failed".into()),
    }
}

/// `account`'s stored login read again, when it holds a token other than `old`.
async fn changed_login(st: &State, account: i64, old: &str) -> Option<Choice> {
    let logins = st.logins.clone();
    match tokio::task::spawn_blocking(move || logins.refresh(account)).await {
        Ok(Ok(choice @ Choice::Token { .. }))
            if choice.account() == account
                && matches!(&choice, Choice::Token { token, .. } if token.as_str() != old) =>
        {
            Some(choice)
        }
        _ => None,
    }
}

/// A renewed login for `account`, whose token `old` Anthropic refused as not signed in (spec 9).
/// Reads the stored login again; when it has not changed, asks for a renewal and reads it every
/// `REFRESH_EVERY` until the hold ends.
/// Only this request waits: others, of this account or another, go on as usual.
async fn renewed_login(st: &State, account: i64, old: &str) -> Option<Choice> {
    if let Some(choice) = changed_login(st, account, old).await {
        st.events.log(&format!(
            "account={account} login refused; a newer one is stored"
        ));
        return Some(choice);
    }
    let logins = st.logins.clone();
    let _ = tokio::task::spawn_blocking(move || logins.renew(account)).await;
    st.events.log(&format!(
        "account={account} login refused; holding for a renewal"
    ));
    let until = tokio::time::Instant::now() + st.options.login_hold;
    loop {
        let left = until.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            st.events.log(&format!(
                "account={account} no renewed login within {} s",
                st.options.login_hold.as_secs()
            ));
            return None;
        }
        tokio::time::sleep(left.min(REFRESH_EVERY)).await;
        if let Some(choice) = changed_login(st, account, old).await {
            st.events.log(&format!("account={account} login renewed"));
            return Some(choice);
        }
    }
}

/// The response for the session: the upstream's status, headers but the hop-by-hop ones, and
/// its body as it arrives.
fn respond(rparts: http::response::Parts, rbody: Incoming, head: bool) -> Response<RelayBody> {
    let mut back = Response::new(rbody.boxed());
    *back.status_mut() = rparts.status;
    let headers = back.headers_mut();
    for (name, value) in &rparts.headers {
        // A HEAD response has no body, so its length is the one the upstream states.
        if relay_proto::forward_response_header(name.as_str())
            || (head && name == header::CONTENT_LENGTH)
        {
            headers.append(name.clone(), value.clone());
        }
    }
    back
}

async fn handle(req: Request<Incoming>, st: &State) -> Response<RelayBody> {
    let route = match req.uri().path_and_query() {
        Some(target) => relay_proto::route(
            req.method().as_str(),
            target.as_str(),
            host_of(&req),
            req.headers().contains_key(header::ORIGIN),
            &st.cfg,
        ),
        None => Route::Refuse,
    };
    let (model, path) = match route {
        Route::Refuse => {
            st.events.log("refused 404");
            return bare(StatusCode::NOT_FOUND);
        }
        // Answered here, before the idle clock: `doctor` asking is not a session's request.
        Route::Version => return version(),
        Route::Model { path } => (true, path),
        Route::Pass { path } => (false, path),
    };
    // Only a request with the relay's secret keeps an idle relay from leaving.
    if let Ok(mut last) = st.last_request.lock() {
        *last = tokio::time::Instant::now();
    }
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let bare_path = path.split('?').next().unwrap_or("").to_string();
    let log = |status: u16, request_id: &str, account: Option<i64>, note: &str| {
        let account = account.map_or("-".to_string(), |a| a.to_string());
        let note = if note.is_empty() {
            String::new()
        } else {
            format!(" {note}")
        };
        st.events.log(&format!(
            "{method} {bare_path} {status} {request_id} account={account}{note}"
        ));
    };

    let Some(uri) = st.upstream.uri(&path) else {
        log(404, "-", None, "bad path");
        return bare(StatusCode::NOT_FOUND);
    };
    let body = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            log(400, "-", None, "request body unreadable");
            return bare(StatusCode::BAD_REQUEST);
        }
    };

    // Read the login last, so a swap while the body arrived still reaches this request.
    let mut choice = None;
    if model && has_bearer(&parts.headers) {
        match login(st).await {
            Ok(c) => choice = Some(c),
            Err(note) => {
                log(503, "-", None, &note);
                return bare(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }
    let session = parts
        .headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let head = parts.method == Method::HEAD;

    // Each pool account is tried at most once (spec 8), and a refused login is renewed at most
    // once per account (spec 9).
    let mut tried: Vec<i64> = Vec::new();
    let mut renewed: Vec<i64> = Vec::new();
    loop {
        let (account, mut token) = match choice.take() {
            Some(Choice::PassThrough { account }) => (Some(account), None),
            Some(Choice::Token { account, token }) => (Some(account), Some(token)),
            None => (None, None),
        };
        tried.extend(account);

        let mut out = Request::new(Full::new(body.clone()));
        *out.method_mut() = parts.method.clone();
        *out.uri_mut() = uri.clone();
        let headers = out.headers_mut();
        for (name, value) in &parts.headers {
            if relay_proto::forward_request_header(name.as_str(), token.is_some()) {
                headers.append(name.clone(), value.clone());
            }
        }
        if let Some(token) = &token {
            let bearer = Zeroizing::new(format!("Bearer {}", token.as_str()));
            let Ok(mut value) = HeaderValue::from_str(&bearer) else {
                log(503, "-", account, "the stored login is not a valid header");
                return bare(StatusCode::SERVICE_UNAVAILABLE);
            };
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }

        let resp = match st.upstream.client.request(out).await {
            Ok(r) => r,
            Err(e) => {
                // The kind only: the error's text names addresses and causes, not for the store.
                let failure = if e.is_connect() { "connect" } else { "other" };
                log(502, "-", account, &format!("upstream: {failure}"));
                let events = st.events.clone();
                let _ = tokio::task::spawn_blocking(move || events.unreachable(failure)).await;
                return bare(StatusCode::BAD_GATEWAY);
            }
        };
        let (rparts, rbody) = resp.into_parts();
        let status = rparts.status.as_u16();
        let request_id = loggable(rparts.headers.get("request-id"));
        log(status, &request_id, account, &usage_note(&rparts.headers));

        let Some(account) = account else {
            return respond(rparts, rbody, head);
        };
        let events = st.events.clone();
        let reported_session = session.clone();
        let headers: Vec<(String, String)> = rparts
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let model_body = body.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let model_name = model_of(&model_body);
            events.response(
                account,
                reported_session.as_deref(),
                model_name.as_deref(),
                status,
                &headers,
            );
        })
        .await;

        // Refused at a usage limit, and nothing sent back yet: swap, and send it again on the
        // account that is active now (spec 8).
        let unified = rparts
            .headers
            .get("anthropic-ratelimit-unified-status")
            .and_then(|v| v.to_str().ok());
        let at_limit = relay_proto::is_limit_refusal(status, unified);
        // Refused as not signed in with the stored login: send it again with a renewed one, or
        // swap away when none comes (spec 9). A session's own login is Claude Code's to renew.
        let old = token
            .take()
            .filter(|_| status == 401 && !renewed.contains(&account));
        if !at_limit && old.is_none() {
            return respond(rparts, rbody, head);
        }
        if let Some(old) = old {
            renewed.push(account);
            if let Some(fresh) = renewed_login(st, account, old.as_str()).await {
                drop(rbody);
                choice = Some(fresh);
                continue;
            }
        }
        let events = st.events.clone();
        let swapped = tokio::task::spawn_blocking(move || {
            if at_limit {
                events.refused_at_limit(account)
            } else {
                events.dead_login(account)
            }
        })
        .await
        .unwrap_or(Swapped::No);
        let Swapped::To(next) = swapped else {
            return respond(rparts, rbody, head);
        };
        if tried.contains(&next) {
            return respond(rparts, rbody, head);
        }
        // Another swap may come between: the login read now decides, and it too is tried once.
        let next = match login(st).await {
            Ok(c) if !tried.contains(&c.account()) => c,
            _ => return respond(rparts, rbody, head),
        };
        let events = st.events.clone();
        let to = next.account();
        let _ = tokio::task::spawn_blocking(move || events.resent(account, to)).await;
        drop(rbody);
        choice = Some(next);
    }
}
