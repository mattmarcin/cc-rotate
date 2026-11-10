//! The relay against a local fake of Anthropic's API that records every request it gets.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ccr_core::relay::RelayConfig;
use ccr_relay::{Choice, Logins, RelayEvents, Upstream};
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use zeroize::Zeroizing;

const S: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// One request as the fake upstream got it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Default)]
struct FakeState {
    seen: Mutex<Vec<Seen>>,
    release: Notify,
    cancelled_at: Mutex<Option<Instant>>,
}

struct Fake {
    addr: SocketAddr,
    state: Arc<FakeState>,
}

impl Fake {
    fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }
}

type FakeBody = BoxBody<Bytes, Infallible>;

/// A body whose chunks come from a channel; the sender sees an error once the body is dropped.
fn chunk_stream(rx: mpsc::Receiver<Bytes>) -> FakeBody {
    struct ChanBody(mpsc::Receiver<Bytes>);
    impl hyper::body::Body for ChanBody {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            self.0.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b))))
        }
    }
    BodyExt::boxed(ChanBody(rx))
}

async fn fake_handle(
    req: Request<Incoming>,
    state: Arc<FakeState>,
) -> Result<Response<FakeBody>, Infallible> {
    let method = req.method().to_string();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let headers = req.headers().clone();
    let body = req.into_body().collect().await.unwrap().to_bytes();
    state.seen.lock().unwrap().push(Seen {
        method,
        path: path.clone(),
        headers,
        body,
    });
    let sse = |body: FakeBody| {
        Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("request-id", "req_sse")
            .body(body)
            .unwrap()
    };
    if path.contains("sse=gated") {
        let (tx, rx) = mpsc::channel(4);
        let st = state.clone();
        tokio::spawn(async move {
            tx.send(Bytes::from("event: a\n\n")).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(10), st.release.notified()).await;
            let _ = tx.send(Bytes::from("event: z\n\n")).await;
        });
        return Ok(sse(chunk_stream(rx)));
    }
    if path.contains("sse=forever") {
        let (tx, rx) = mpsc::channel(1);
        let st = state.clone();
        tokio::spawn(async move {
            for i in 0..400 {
                if tx
                    .send(Bytes::from(format!("data: {i}\n\n")))
                    .await
                    .is_err()
                {
                    *st.cancelled_at.lock().unwrap() = Some(Instant::now());
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        });
        return Ok(sse(chunk_stream(rx)));
    }
    if path.contains("sse=slow") {
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            for i in 0..3 {
                let _ = tx.send(Bytes::from(format!("data: {i}\n\n"))).await;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
        return Ok(sse(chunk_stream(rx)));
    }
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("request-id", "req_1")
        .header("anthropic-ratelimit-unified-5h-utilization", "0.25")
        .header("connection", "keep-alive")
        .body(BodyExt::boxed(Full::new(Bytes::from("{\"ok\":true}"))))
        .unwrap())
}

async fn fake_upstream() -> Fake {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(FakeState::default());
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let st = st.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| fake_handle(req, st.clone()));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    Fake { addr, state }
}

/// Logins whose choice a test can change.
struct Switch(Mutex<Result<Choice, String>>);

impl Switch {
    fn new(choice: Choice) -> Arc<Switch> {
        Arc::new(Switch(Mutex::new(Ok(choice))))
    }
    fn set(&self, choice: Choice) {
        *self.0.lock().unwrap() = Ok(choice);
    }
}

impl Logins for Switch {
    fn for_model(&self) -> anyhow::Result<Choice> {
        match &*self.0.lock().unwrap() {
            Ok(c) => Ok(c.clone()),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}

fn token(account: i64, t: &str) -> Choice {
    Choice::Token {
        account,
        token: Zeroizing::new(t.to_string()),
    }
}

type ResponseCall = (
    i64,
    Option<String>,
    Option<String>,
    u16,
    Vec<(String, String)>,
);

#[derive(Default)]
struct Recorder {
    responses: Mutex<Vec<ResponseCall>>,
    lines: Mutex<Vec<String>>,
}

impl RelayEvents for Recorder {
    fn response(
        &self,
        account: i64,
        session: Option<&str>,
        model: Option<&str>,
        status: u16,
        headers: &[(String, String)],
    ) {
        self.responses.lock().unwrap().push((
            account,
            session.map(str::to_string),
            model.map(str::to_string),
            status,
            headers.to_vec(),
        ));
    }
    fn log(&self, line: &str) {
        self.lines.lock().unwrap().push(line.to_string());
    }
}

struct Relay {
    cfg: RelayConfig,
    events: Arc<Recorder>,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl Relay {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}/r/{S}{path}", self.cfg.port)
    }

    fn lines(&self) -> Vec<String> {
        self.events.lines.lock().unwrap().clone()
    }

    async fn send(&self, req: Request<Full<Bytes>>) -> Response<Incoming> {
        self.client.request(req).await.unwrap()
    }
}

async fn relay_to(addr: SocketAddr, logins: Arc<dyn Logins>) -> Relay {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cfg = RelayConfig {
        port: listener.local_addr().unwrap().port(),
        secret: S.into(),
    };
    let events = Arc::new(Recorder::default());
    let upstream = Upstream::for_test(format!("http://{addr}").parse().unwrap()).unwrap();
    tokio::spawn(ccr_relay::serve(
        listener,
        cfg.clone(),
        upstream,
        logins,
        events.clone(),
    ));
    let client = Client::builder(TokioExecutor::new()).build_http();
    Relay {
        cfg,
        events,
        client,
    }
}

fn model_request(relay: &Relay, query: &str, auth: Option<&str>) -> Request<Full<Bytes>> {
    let mut b = Request::post(relay.url(&format!("/v1/messages{query}")))
        .header("content-type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("x-claude-code-session-id", "sess-1")
        .header("x-multi", "one")
        .header("x-multi", "two");
    if let Some(a) = auth {
        b = b.header("authorization", a);
    }
    b.body(Full::new(Bytes::from(
        r#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}]}"#,
    )))
    .unwrap()
}

async fn body_of(resp: Response<Incoming>) -> Bytes {
    resp.into_body().collect().await.unwrap().to_bytes()
}

fn values<'a>(h: &'a HeaderMap, name: &str) -> Vec<&'a [u8]> {
    h.get_all(name).iter().map(|v| v.as_bytes()).collect()
}

/// Every header the client sent, apart from those the relay replaces, arrives with the same values.
fn assert_same_headers_except(sent: &HeaderMap, got: &HeaderMap, except: &[&str]) {
    for name in sent.keys() {
        if except.contains(&name.as_str()) {
            continue;
        }
        assert_eq!(
            values(sent, name.as_str()),
            values(got, name.as_str()),
            "header {name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn model_request_gets_pool_token() {
    let fake = fake_upstream().await;
    let relay = relay_to(fake.addr, Switch::new(token(2, "tok-b"))).await;
    let req = model_request(&relay, "?beta=true", Some("Bearer home"));
    let sent_headers = req.headers().clone();
    let resp = relay.send(req).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["request-id"], "req_1");
    assert!(resp.headers().get("connection").is_none());
    assert_eq!(&body_of(resp).await[..], b"{\"ok\":true}");

    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    let got = &seen[0];
    assert_eq!(got.method, "POST");
    assert_eq!(got.path, "/v1/messages?beta=true");
    assert_eq!(
        values(&got.headers, "authorization"),
        vec![&b"Bearer tok-b"[..]]
    );
    assert_eq!(got.headers["host"], fake.addr.to_string().as_str());
    assert_same_headers_except(&sent_headers, &got.headers, &["authorization", "host"]);
    assert_eq!(
        &got.body[..],
        br#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}]}"#
    );

    let responses = relay.events.responses.lock().unwrap().clone();
    assert_eq!(responses.len(), 1);
    let (account, session, model, status, headers) = &responses[0];
    assert_eq!(*account, 2);
    assert_eq!(session.as_deref(), Some("sess-1"));
    assert_eq!(model.as_deref(), Some("claude-x"));
    assert_eq!(*status, 200);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "anthropic-ratelimit-unified-5h-utilization" && v == "0.25")
    );

    let lines = relay.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    for part in ["POST", "/v1/messages", "200", "req_1", "account=2"] {
        assert!(line.contains(part), "{line}");
    }
    for secret in ["tok-b", "home", S, "beta=true", "claude-x", "sess-1"] {
        assert!(!line.contains(secret), "{line} has {secret}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pass_through_keeps_own_login() {
    let fake = fake_upstream().await;
    let logins = Switch::new(token(2, "tok-b"));
    let relay = relay_to(fake.addr, logins.clone()).await;

    let req = Request::head(relay.url("/api/hello"))
        .header("authorization", "Bearer home")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(relay.send(req).await.status(), 200);

    logins.set(Choice::PassThrough { account: 1 });
    let req = model_request(&relay, "", Some("Bearer home"));
    let sent_headers = req.headers().clone();
    assert_eq!(relay.send(req).await.status(), 200);

    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].method, "HEAD");
    assert_eq!(seen[0].path, "/api/hello");
    assert_eq!(seen[0].headers["authorization"], "Bearer home");
    assert_eq!(seen[1].path, "/v1/messages");
    assert_eq!(
        values(&seen[1].headers, "authorization"),
        vec![&b"Bearer home"[..]]
    );
    assert_same_headers_except(&sent_headers, &seen[1].headers, &["host"]);

    // A request that is not a model request reports no account; the pass-through one reports home.
    let responses = relay.events.responses.lock().unwrap().clone();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].0, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_key_session_unchanged() {
    let fake = fake_upstream().await;
    let relay = relay_to(fake.addr, Switch::new(token(2, "tok-b"))).await;

    let mut req = model_request(&relay, "", None);
    req.headers_mut()
        .insert("x-api-key", "sk-ant-api-key".parse().unwrap());
    let sent_headers = req.headers().clone();
    assert_eq!(relay.send(req).await.status(), 200);

    let req = model_request(&relay, "", Some("Basic abc"));
    assert_eq!(relay.send(req).await.status(), 200);

    let seen = fake.seen();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].headers.get("authorization").is_none());
    assert_eq!(seen[0].headers["x-api-key"], "sk-ant-api-key");
    assert_same_headers_except(&sent_headers, &seen[0].headers, &["host"]);
    assert_eq!(
        values(&seen[1].headers, "authorization"),
        vec![&b"Basic abc"[..]]
    );
    assert!(relay.events.responses.lock().unwrap().is_empty());
    for line in relay.lines() {
        assert!(
            !line.contains("sk-ant-api-key") && !line.contains("abc"),
            "{line}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_never_reaches_upstream() {
    let fake = fake_upstream().await;
    let relay = relay_to(fake.addr, Switch::new(token(2, "tok-b"))).await;
    let port = relay.cfg.port;
    let wrong_secret = "f".repeat(64);
    let cases: Vec<Request<Full<Bytes>>> = vec![
        Request::post(format!(
            "http://127.0.0.1:{port}/r/{wrong_secret}/v1/messages"
        ))
        .header("authorization", "Bearer home")
        .body(Full::new(Bytes::new()))
        .unwrap(),
        Request::post(format!("http://127.0.0.1:{port}/v1/messages"))
            .body(Full::new(Bytes::new()))
            .unwrap(),
        Request::post(relay.url("/v1/messages"))
            .header("origin", "https://example.com")
            .body(Full::new(Bytes::new()))
            .unwrap(),
        Request::post(relay.url("/v1/messages"))
            .header("host", format!("localhost:{port}"))
            .body(Full::new(Bytes::new()))
            .unwrap(),
        Request::put(relay.url("/v1/messages"))
            .body(Full::new(Bytes::new()))
            .unwrap(),
    ];
    for req in cases {
        let resp = relay.send(req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(body_of(resp).await.is_empty());
    }

    // An absolute-form request line naming another host is refused too.
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let raw = format!(
        "GET http://evil.example/r/{S}/api/hello HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    tcp.write_all(raw.as_bytes()).await.unwrap();
    let mut out = String::new();
    tcp.read_to_string(&mut out).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 404"), "{out}");

    assert!(fake.seen().is_empty());
    let lines = relay.lines();
    assert_eq!(lines.len(), 6, "{lines:?}");
    for line in lines {
        assert!(line.contains("refused"), "{line}");
        for detail in [
            wrong_secret.as_str(),
            S,
            "example",
            "localhost",
            "messages",
            "hello",
        ] {
            assert!(!line.contains(detail), "{line} has {detail}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_login_answers_with_an_error_and_forwards_nothing() {
    let fake = fake_upstream().await;
    let logins = Arc::new(Switch(Mutex::new(Err("no active account".into()))));
    let relay = relay_to(fake.addr, logins).await;
    let resp = relay
        .send(model_request(&relay, "", Some("Bearer home")))
        .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(fake.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_down_is_a_bad_gateway() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let relay = relay_to(closed, Switch::new(token(2, "tok-b"))).await;
    let resp = relay
        .send(model_request(&relay, "", Some("Bearer home")))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let lines = relay.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("502") && !lines[0].contains("tok-b"));
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_incrementally() {
    let fake = fake_upstream().await;
    let relay = relay_to(fake.addr, Switch::new(token(2, "tok-b"))).await;
    let resp = relay
        .send(model_request(&relay, "?sse=gated", Some("Bearer home")))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut body = resp.into_body();
    let first = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("the first event should arrive before the last is sent")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(&first[..], b"event: a\n\n");
    fake.state.release.notify_one();
    let rest = tokio::time::timeout(Duration::from_secs(5), body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(&rest[..], b"event: z\n\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_cancels_upstream() {
    let fake = fake_upstream().await;
    let relay = relay_to(fake.addr, Switch::new(token(2, "tok-b"))).await;
    let resp = relay
        .send(model_request(&relay, "?sse=forever", Some("Bearer home")))
        .await;
    let mut body = resp.into_body();
    let first = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.is_data());
    drop(body);
    drop(relay.client);
    let dropped = Instant::now();
    let deadline = dropped + Duration::from_secs(3);
    let cancelled = loop {
        if let Some(at) = *fake.state.cancelled_at.lock().unwrap() {
            break at;
        }
        assert!(Instant::now() < deadline, "the upstream stream never ended");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        cancelled.duration_since(dropped) < Duration::from_secs(1),
        "took {:?}",
        cancelled.duration_since(dropped)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_mid_stream_keeps_stream() {
    let fake = fake_upstream().await;
    let logins = Switch::new(token(2, "tok-a"));
    let relay = relay_to(fake.addr, logins.clone()).await;
    let resp = relay
        .send(model_request(&relay, "?sse=slow", Some("Bearer home")))
        .await;
    let mut body = resp.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&first[..], b"data: 0\n\n");

    logins.set(token(3, "tok-b"));
    let rest = tokio::time::timeout(Duration::from_secs(5), body.collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    assert_eq!(&rest[..], b"data: 1\n\ndata: 2\n\n");

    let resp = relay
        .send(model_request(&relay, "", Some("Bearer home")))
        .await;
    assert_eq!(resp.status(), 200);
    let seen = fake.seen();
    assert_eq!(seen[0].headers["authorization"], "Bearer tok-a");
    assert_eq!(seen[1].headers["authorization"], "Bearer tok-b");
    let accounts: Vec<i64> = relay
        .events
        .responses
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.0)
        .collect();
    assert_eq!(accounts, vec![2, 3]);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests() {
    let fake = fake_upstream().await;
    let relay = Arc::new(relay_to(fake.addr, Switch::new(token(2, "tok-b"))).await);
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..50 {
        let relay = relay.clone();
        set.spawn(async move {
            let resp = relay
                .send(model_request(&relay, "", Some("Bearer home")))
                .await;
            (resp.status(), body_of(resp).await)
        });
    }
    let mut n = 0;
    while let Some(r) = set.join_next().await {
        let (status, body) = r.unwrap();
        assert_eq!(status, 200);
        assert_eq!(&body[..], b"{\"ok\":true}");
        n += 1;
    }
    assert_eq!(n, 50);
    let seen = fake.seen();
    assert_eq!(seen.len(), 50);
    assert!(
        seen.iter()
            .all(|s| s.headers["authorization"] == "Bearer tok-b")
    );
}

/// Sends one HTTP/2 request (prior knowledge, no `Host` header) to the relay over a new connection.
async fn h2_request(port: u16, uri: &str) -> StatusCode {
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut send, conn) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
            .await
            .unwrap();
    tokio::spawn(conn);
    let req = Request::get(uri)
        .version(http::Version::HTTP_2)
        .header("authorization", "Bearer home")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert!(req.headers().get("host").is_none());
    let resp = send.send_request(req).await.unwrap();
    let status = resp.status();
    body_of(resp).await;
    status
}

#[tokio::test(flavor = "multi_thread")]
async fn http2_authority_is_the_host() {
    let fake = fake_upstream().await;
    let relay = relay_to(fake.addr, Switch::new(token(2, "tok-b"))).await;
    let port = relay.cfg.port;

    let status = h2_request(port, &relay.url("/api/hello?x=1")).await;
    assert_eq!(status, StatusCode::OK);
    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/api/hello?x=1");
    assert_eq!(seen[0].headers["authorization"], "Bearer home");

    for uri in [
        format!("http://evil.example/r/{S}/api/hello"),
        format!("http://localhost:{port}/r/{S}/api/hello"),
        format!("http://127.0.0.1:{port}/r/{}/api/hello", "f".repeat(64)),
    ] {
        assert_eq!(h2_request(port, &uri).await, StatusCode::NOT_FOUND, "{uri}");
    }
    assert_eq!(fake.seen().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_authorization_headers_get_no_pool_token() {
    let fake = fake_upstream().await;
    let relay = relay_to(fake.addr, Switch::new(token(2, "tok-b"))).await;
    let mut req = model_request(&relay, "", Some("Bearer home"));
    req.headers_mut()
        .append("authorization", "Bearer other".parse().unwrap());
    let sent_headers = req.headers().clone();
    assert_eq!(relay.send(req).await.status(), 200);

    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        values(&seen[0].headers, "authorization"),
        vec![&b"Bearer home"[..], &b"Bearer other"[..]]
    );
    assert_same_headers_except(&sent_headers, &seen[0].headers, &["host"]);
    assert!(relay.events.responses.lock().unwrap().is_empty());
}
