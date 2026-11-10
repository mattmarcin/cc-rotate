//! The loopback relay of relay mode: a reverse proxy from `127.0.0.1:<port>` to Anthropic's API
//! that puts the active pool account's login on model requests.
//!
//! The design is in `docs/specs/2026-11-10-relay-mode.md`, sections 5 and 13.
//! The rules for which requests pass and which headers go on are in `ccr_core::relay_proto`.

pub mod login;

use std::borrow::Cow;
use std::convert::Infallible;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
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

pub use login::StoreLogins;

/// The only address the relay sends requests to in a release build.
const ANTHROPIC: &str = "https://api.anthropic.com";

/// The header Claude Code names its session in.
const SESSION_HEADER: &str = "x-claude-code-session-id";

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

impl std::fmt::Debug for Choice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Choice::PassThrough { account } => write!(f, "PassThrough {{ account: {account} }}"),
            Choice::Token { account, .. } => write!(f, "Token {{ account: {account}, .. }}"),
        }
    }
}

/// Where the relay gets the login for a model request.
/// Called once per model request, on a thread that may block.
pub trait Logins: Send + Sync {
    fn for_model(&self) -> Result<Choice>;
}

/// What the relay reports as it works. Every method does nothing unless implemented.
pub trait RelayEvents: Send + Sync {
    /// A model request that went out with a known account got its response's status and headers.
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
    upstream: Upstream,
    logins: Arc<dyn Logins>,
    events: Arc<dyn RelayEvents>,
}

type RelayBody = BoxBody<Bytes, hyper::Error>;

/// Serve requests from `listener`, which must be bound to 127.0.0.1, until the process ends.
pub async fn serve(
    listener: std::net::TcpListener,
    cfg: RelayConfig,
    upstream: Upstream,
    logins: Arc<dyn Logins>,
    events: Arc<dyn RelayEvents>,
) -> Result<()> {
    let addr = listener.local_addr()?;
    if addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        bail!("the relay listens on 127.0.0.1 only, not {}", addr.ip());
    }
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let state = Arc::new(State {
        cfg,
        upstream,
        logins,
        events,
    });
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Out of file descriptors, for one: wait for some to close.
                state.events.log(&format!("accept failed: {e}"));
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(req, &state).await) }
            });
            let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            // The timer lets hyper drop a connection whose request headers never finish.
            builder.http1().timer(TokioTimer::new());
            let _ = builder
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// `serve` on a runtime of its own, for `cc-rotate-ctl relay run`.
pub fn run_blocking(
    listener: std::net::TcpListener,
    cfg: RelayConfig,
    upstream: Upstream,
    logins: Arc<dyn Logins>,
    events: Arc<dyn RelayEvents>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the relay's runtime")?;
    runtime.block_on(serve(listener, cfg, upstream, logins, events))
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
        Route::Model { path } => (true, path),
        Route::Pass { path } => (false, path),
    };
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
    let mut account = None;
    let mut token = None;
    if model && has_bearer(&parts.headers) {
        let logins = st.logins.clone();
        match tokio::task::spawn_blocking(move || logins.for_model()).await {
            Ok(Ok(Choice::PassThrough { account: a })) => account = Some(a),
            Ok(Ok(Choice::Token {
                account: a,
                token: t,
            })) => {
                account = Some(a);
                token = Some(t);
            }
            Ok(Err(e)) => {
                log(503, "-", None, &format!("no login: {e:#}"));
                return bare(StatusCode::SERVICE_UNAVAILABLE);
            }
            Err(_) => {
                log(503, "-", None, "no login: the lookup failed");
                return bare(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }

    let mut out = Request::new(Full::new(body.clone()));
    *out.method_mut() = parts.method.clone();
    *out.uri_mut() = uri;
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
    drop(token);

    let resp = match st.upstream.client.request(out).await {
        Ok(r) => r,
        Err(e) => {
            log(502, "-", account, &format!("upstream: {e}"));
            return bare(StatusCode::BAD_GATEWAY);
        }
    };
    let (rparts, rbody) = resp.into_parts();
    let request_id = loggable(rparts.headers.get("request-id"));
    log(rparts.status.as_u16(), &request_id, account, "");

    if let Some(account) = account {
        let events = st.events.clone();
        let session = parts
            .headers
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let status = rparts.status.as_u16();
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
        let _ = tokio::task::spawn_blocking(move || {
            let model_name = model_of(&body);
            events.response(
                account,
                session.as_deref(),
                model_name.as_deref(),
                status,
                &headers,
            );
        })
        .await;
    }

    let mut back = Response::new(rbody.boxed());
    *back.status_mut() = rparts.status;
    let head = parts.method == Method::HEAD;
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
