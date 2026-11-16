# Relay Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An opt-in relay mode in which the cc-rotate profile keeps the desktop app's account's login and a loopback relay sends every session's model requests on the pool account in use, so artifacts from any session open in the app's artifact pane.

**Architecture:** Pure decisions (routing, header rules, readings, limit refusals, renewal timing, launcher environment) live in `ccr-core` and are unit-tested. The relay's network code lives in a new `crates/ccr-relay` library (tokio + hyper + rustls), run by the hidden `cc-rotate-ctl relay run`. Swaps, checks, the status line and both launchers branch on relay mode through one helper, `relay::home`.

**Tech Stack:** Rust 2024 (workspace `rust-version = "1.89"`), rusqlite store, tokio 1, hyper 1 + hyper-util 0.1 + http-body-util 0.1, hyper-rustls 0.27 with its `rustls-platform-verifier` feature, Python/Node end-to-end tests.

**Spec:** `docs/specs/2026-11-10-relay-mode.md` (cited as "spec N"); the main spec `docs/specs/2026-10-24-cc-rotate.md` is "main N".

## Global Constraints

- Relay mode is off by default (`relay_mode` unset = off); with it off, behaviour is exactly the main spec's.
- Session variables, verbatim: `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/r/<secret>` and `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1`.
- Port: a free port on 127.0.0.1 in 49152–65535; secret: 256 random bits, hex. Both in `<data dir>/relay.json`, mode 0600.
- Listen on `127.0.0.1` only. Upstream is the constant `https://api.anthropic.com`; only a test build (cargo feature `relay-test-upstream`) may point it elsewhere.
- Model requests are exactly `POST /v1/messages` and `POST /v1/messages/count_tokens` (any query). Only they get a pool login.
- Never log a token, a body, or a request header's value.
- `relay_renew_margin_s` default 900; renewal 401 hold 30 s; idle exit after `relay off` 3600 s; upgrade check every 60 s.
- Claude Code renews logins; cc-rotate never calls a sign-in endpoint.
- Launchers keep their time budget: they never wait for the relay.
- Commits only with the user's go-ahead; never add Claude attribution to a commit or PR.
- Doc prose follows the repo's style: one sentence per line, plain words.

## Review Focus

- A session that sends a second model request while the first is being resent at a limit: both must end on the new account, and the swap must run once (Task 8: `concurrent_refusals_swap_once`).
- A swap committed while a long response streams: the stream finishes on the old account and is never cut (Task 7: `swap_mid_stream_keeps_stream`).
- `relay on` interrupted between the profile write and the commit (crash): `finish_dead_swap` must settle it as a mode change, not as a swap (Task 5: `dead_relay_on_settles_mode`).
- A desktop session started while `relay.json` is missing or unreadable with the mode on: it must start with no relay variables and the error logged, not fail to start (Task 3: `launch_env_missing_config`).
- The home account removed or signed out while relay mode is on: `relay`/`doctor` must say so and swaps must not pick a target that leaves no profile login (Task 11: `home_signed_out_fails`).

---

### Task 1: Relay mode state and `relay.json`

**Files:**
- Create: `crates/ccr-core/src/relay.rs`
- Modify: `crates/ccr-core/src/lib.rs` (add `pub mod relay;`), `crates/ccr-core/src/paths.rs` (three paths), `crates/ccr-core/src/store.rs` (`Settings.relay_renew_margin_s`, `SETTING_KEYS`, `validate_setting`)

**Interfaces:**
- Produces:
  - `Paths::relay_config(&self) -> PathBuf` (`<data dir>/relay.json`), `Paths::relay_lock(&self) -> PathBuf` (`<data dir>/relay.lock`), `Paths::relay_log(&self) -> PathBuf` (`<data dir>/logs/relay.log`)
  - `relay::SETTING_MODE = "relay_mode"`, `relay::SETTING_HOME = "home_account"` (written only by `relay::set_mode`, not in `SETTING_KEYS`)
  - `relay::home(store: &Store) -> Result<Option<i64>>`: `Some(home id)` when the mode is `on`
  - `relay::profile_account(store: &Store) -> Result<Option<Account>>`: the home account in relay mode, else `store.active_account()`
  - `relay::set_mode(store: &Store, home: Option<i64>) -> Result<()>` (writes both settings rows; `None` deletes them)
  - `relay::RelayConfig { port: u16, secret: String }` with `read(path: &Path) -> Result<Option<RelayConfig>>`, `create(path: &Path) -> Result<RelayConfig>`, `base_url(&self) -> String`
  - `relay::ENV_BASE_URL = "ANTHROPIC_BASE_URL"`, `relay::ENV_FIRST_PARTY = "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL"`, `relay::session_env(cfg: &RelayConfig) -> [(&'static str, String); 2]`
  - `Settings.relay_renew_margin_s: i64` (default 900, clamp 60..=3600, a normal `config` key)

- [ ] **Step 1: Write failing tests** in `relay.rs`'s `mod tests`:
  - `home_off_by_default`: fresh `Store::open_in_memory()` → `relay::home` is `None`, `profile_account` equals `active_account`.
  - `home_on`: `set_mode(store, Some(id))` → `home == Some(id)`, `profile_account` is that account even when another is active.
  - `config_roundtrip`: `create(tmp/relay.json)` → `port` in 49152..=65535, `secret.len() == 64` hex, file mode `0o600` on Unix; `read` returns an equal value; `base_url() == format!("http://127.0.0.1:{port}/r/{secret}")`.
  - `session_env_values`: `session_env` returns exactly `[("ANTHROPIC_BASE_URL", base_url), ("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", "1")]`.
  - In `store.rs` tests: `relay_renew_margin_s` default 900; `set_setting("relay_renew_margin_s", "10")` rejected; `set_setting("relay_mode", "on")` rejected as unknown.
- [ ] **Step 2: Run** `cargo test -p ccr-core relay:: store::tests::relay` → FAIL (module missing).
- [ ] **Step 3: Implement.** `create` binds `127.0.0.1:<random>` (from `getrandom`) to prove the port free, up to 50 tries, then writes the JSON through `secret::` private-file helpers (write temp, rename). `set_mode` writes the settings table directly (bypassing `set_setting`'s key check).
- [ ] **Step 4: Run** the same tests → PASS; `cargo clippy -p ccr-core --all-targets -- -D warnings` clean.
- [ ] **Step 5: Commit** `feat(relay): relay mode state and relay.json`.

### Task 2: Pure relay protocol rules

**Files:**
- Create: `crates/ccr-core/src/relay_proto.rs` (+ `pub mod relay_proto;` in `lib.rs`)
- Modify: `crates/ccr-core/src/scheduler.rs` (add `Reason::LimitRefused`, `as_str` `"rate_limit"`)

**Interfaces:**
- Consumes: `RelayConfig` (Task 1), `SampleWindow` (store).
- Produces:
  - `enum Route { Refuse, Model { path: String }, Pass { path: String } }` and `route(method: &str, path_and_query: &str, host: Option<&str>, has_origin: bool, cfg: &RelayConfig) -> Route` (spec 5 step 1–2; `path` is without the `/r/<secret>` prefix, query kept)
  - `forward_request_header(name: &str, model: bool) -> bool`: false for `host`, hop-by-hop (`connection`, `keep-alive`, `proxy-connection`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`), `content-length`, and for `authorization` when `model`
  - `forward_response_header(name: &str) -> bool`: false for hop-by-hop and `content-length`
  - `window_name(header_window: &str) -> String`: `5h`→`five_hour`, `7d`→`seven_day`, `7d_oi`→`seven_day_overage_included`, `overage`→`overage`, anything else unchanged (Claude Code 2.1.322's table)
  - `readings<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<SampleWindow>`: one window per `anthropic-ratelimit-unified-<w>-utilization`, `pct = value × 100`, `resets_at` from `-<w>-reset` when it parses
  - `is_limit_refusal(status: u16, unified_status: Option<&str>) -> bool`: `status == 429 && unified_status == Some("rejected")`
  - `renew_due(expires_at_ms: i64, now_ms: i64, margin_s: i64) -> bool`: `expires_at_ms - now_ms <= margin_s * 1000`

- [ ] **Step 1: Write failing tests**:
  - `route_model`: `POST /r/<s>/v1/messages?beta=true` host `127.0.0.1:<port>` → `Model { path: "/v1/messages?beta=true" }`; same for `/v1/messages/count_tokens`.
  - `route_pass`: `HEAD /r/<s>/api/hello` → `Pass { path: "/api/hello" }`; `GET /r/<s>/v1/models` → `Pass`.
  - `route_refuse`: wrong secret, no prefix, `/r/<s>` prefix of a longer segment (`/r/<s>x/v1/messages`), host `localhost:<port>`, host missing, `has_origin`, method `PUT` → `Refuse`; `GET /r/<s>/v1/messages` → `Pass` (not a model request).
  - `headers_rules`: `forward_request_header("Authorization", true) == false`, `("authorization", false) == true`, `("anthropic-beta", true) == true`, `("Host", false) == false`.
  - `readings_from_real_headers`: the control run's headers (spec F37: 5h `0.01` reset `1794347400`, 7d `0.0` reset `1794499200`) → `[five_hour 1.0 Some(1794347400), seven_day 0.0 Some(1794499200)]`; an unknown `7d_fable` window is kept under its own name; a missing reset gives `None`; a non-number utilization is skipped.
  - `limit_refusal`: `(429, Some("rejected"))` true; `(429, Some("allowed"))`, `(429, None)`, `(529, Some("rejected"))` false.
  - `renew_timing`: `renew_due(now + 901_000, now, 900)` false, `(now + 900_000, now, 900)` true.
- [ ] **Step 2: Run** `cargo test -p ccr-core relay_proto` → FAIL.
- [ ] **Step 3: Implement** the functions in `relay_proto.rs`; compare the secret with a constant-time equality.
- [ ] **Step 4: Run** → PASS; clippy clean.
- [ ] **Step 5: Commit** `feat(relay): request routing, header and reading rules`.

### Task 3: Launchers add the relay's variables

**Files:**
- Modify: `crates/ccr-core/src/relay.rs` (read-only launch env), `crates/ccr-core/src/desktop.rs:31-72` (`LaunchPlan.set`), `crates/ccr-cli/src/desktop_launch.rs`, `crates/ccr-cli/src/launcher.rs`, `crates/ccr-cli/src/process.rs` (`spawn_relay`)

**Interfaces:**
- Consumes: Task 1.
- Produces:
  - `relay::launch_env(data_dir: &Path) -> Result<Option<[(&'static str, String); 2]>>`: reads `relay_mode` with `Store::read_setting_any_schema` and `relay.json`; `Ok(None)` when off; `Err` when on but `relay.json` is missing or unreadable
  - `LaunchPlan { config_dir, remove, set: Vec<(&'static str, String)> }`; `plan_launch(args, var, rotate_profile, home, relay_env: Option<[(&'static str, String); 2]>)` puts `relay_env` in `set` only when `config_dir == Some(rotate_profile)`
  - `relay::answers(port: u16) -> bool`: a TCP connect to 127.0.0.1 within 50 ms
  - `process::spawn_relay() -> Result<()>`: starts `cc-rotate-ctl relay run` detached, like `spawn_swap`

- [ ] **Step 1: Write failing tests**:
  - `desktop::tests::relay_env_set_for_pool_session`: desktop env, `relay_env = Some(..)` → `plan.set` holds both pairs.
  - `desktop::tests::relay_env_not_for_resumed_old_session`: the resumed-older-session case (existing fixture) → `plan.set` empty.
  - `desktop::tests::relay_env_not_for_non_desktop`: not a desktop session → empty.
  - `relay::tests::launch_env_off` (no setting → `None`), `launch_env_on` (setting + file → the pairs), `launch_env_missing_config` (setting on, no file → `Err`).
- [ ] **Step 2: Run** `cargo test -p ccr-core desktop:: relay::tests::launch_env` → FAIL.
- [ ] **Step 3: Implement.** Both launchers: call `launch_env(Paths::default_data_dir()?)`; on `Err`, `process::log_error("relay", ..)` and start without the variables (Review Focus); on `Some`, set the pairs and, when `!relay::answers(port)`, call `spawn_relay()` without waiting (errors logged only). `desktop-launch` still writes nothing to stdout/stderr.
- [ ] **Step 4: Run** the tests → PASS, then `cargo build -p ccr-cli` and the existing integration test of `desktop-launch` (main 24.6) → PASS unchanged with relay mode off.
- [ ] **Step 5: Commit** `feat(relay): launchers point sessions at the relay`.

### Task 4: Checks that renew, and which account a check skips

**Files:**
- Modify: `crates/ccr-core/src/usage_check.rs` (`Trigger::Renew`, `job` account rules), `crates/ccr-cli/src/main.rs` (`check --renew`)

**Interfaces:**
- Consumes: `relay::home` (Task 1).
- Produces: `usage_check::Trigger::Renew`; `usage_check::renew(store: &Store, paths: &Paths, account_id: i64) -> Result<CheckOutcome>` (= `check(.., Trigger::Renew)`); hidden CLI `cc-rotate-ctl check --renew <account>`.

Rules in `job` (spec 9): in relay mode the home account is skipped ("it's the profile's login"), and an `Active` account that is not the home account is checked like a parked one; with the mode off, unchanged. `Trigger::Renew` sets the copied login's `expiresAt` to 0 (as `held_back` does, `usage_check.rs:703`), ignores `not_before` (the renewal precedes the usage fetch), waits up to `lock_timeout_ms` for the account lock, and runs even with `usage_checks` off.

- [ ] **Step 1: Write failing tests** with the existing `check_with` runner seam (a fake `run` that writes a renewed login into the scratch profile):
  - `renew_forces_expiry_zero`: the scratch login handed to `run` has `expiresAt == 0`; the renewed login is stored with `source = "check"`.
  - `relay_checks_active_non_home`: relay mode on, home = A, active = B → `check(B, Background)` runs; `check(A, Manual)` is `Skipped`.
  - `off_mode_unchanged`: mode off → `check(active, Background)` still `Skipped` with the old reason.
  - `renew_ignores_not_before`: `not_before` in the future → `Renew` still runs.
- [ ] **Step 2: Run** `cargo test -p ccr-core usage_check::tests::renew usage_check::tests::relay` → FAIL.
- [ ] **Step 3: Implement** the trigger and rules; add `--renew` (hidden) to `Command::Check`, which runs `usage_check::renew` for the one account given and prints `describe(&outcome)`.
- [ ] **Step 4: Run** → PASS; the whole `usage_check` suite → PASS.
- [ ] **Step 5: Commit** `feat(relay): renewal checks`.

### Task 5: Swaps in relay mode, and turning it on and off

**Files:**
- Modify: `crates/ccr-core/src/swap.rs` (`run`, `PendingSwap`, `finish_dead_swap`, `identify_changed_login`), `crates/ccr-core/src/relay.rs` (`turn_on`, `turn_off`)

**Interfaces:**
- Consumes: Tasks 1, 4.
- Produces:
  - `PendingSwap.relay: Option<RelayChange>` (`#[serde(default)]`), `enum RelayChange { On { home: i64 }, Off }`
  - `swap::write_profile_login(store, paths, outgoing: i64, incoming: i64, change: RelayChange) -> Result<()>`: main 6 steps 4–6 and 8 with the pending mark carrying `change`; its commit transaction calls `relay::set_mode` instead of setting states
  - `relay::turn_on(store, paths, home: i64) -> Result<()>` and `relay::turn_off(store, paths) -> Result<()>` (spec 4, without the service: Task 10 adds it)
  - `fn record_swap(s: &Store, c: &SwapCommit) -> Result<()>`: the bookkeeping now inside `run`'s commit closure (`swap.rs:1360-1413`: states, `left_at_stop`, promotion, `last_swap_at`, `active_since`, reset bookkeeping, event), shared by both paths; `SwapCommit` holds the values that closure reads today

`run` in relay mode (spec 6): no Keychain-locked refusal, no `confirm_active`, no profile identification; after the target is picked and locked, if `renew_due(target.expires_at, now, margin)`, run `usage_check::renew` within the 25 s check budget and pass the target over when it does not end with a valid login; then one `immediate` transaction calling `record_swap` (no `META_LOGIN_WRITTEN_AT`, no `META_CRED_SEEN`, no pending mark). `identify_changed_login` and the "profile's login changed" guard compare with `relay::profile_account`.

- [ ] **Step 1: Write failing tests** (in-memory store, temp profile as the existing swap tests do):
  - `relay_swap_leaves_profile`: mode on, home A active B, swap to C → C active, B parked, the profile's credentials file byte-identical, `swap` event logged, no `swap_pending` meta.
  - `relay_swap_renews_target`: C's `expires_at` within the margin → the renew runner is called once before the commit (inject via a `run` seam parameter on an internal `run_with`).
  - `relay_swap_to_home`: swap to A → A active, profile unchanged.
  - `turn_on_writes_home`: mode off, active B with its login in the profile → after `turn_on(A)`: profile holds A's login, B's profile login saved to B's row, B still active, `home == Some(A)`.
  - `turn_off_writes_active`: mode on, active C → after `turn_off`: profile holds C's login, A's (profile) login saved to A's row, mode off.
  - `dead_relay_on_settles_mode` (Review Focus): pending mark `{relay: On{home: A}}` left with the profile holding A's login → `finish_dead_swap` sets the mode on and leaves states alone; with the profile holding B's login → clears the mark, mode off.
  - `turn_on_refused_when_pending`: a pending swap → `turn_on` errors.
- [ ] **Step 2: Run** `cargo test -p ccr-core swap::tests::relay relay::tests::turn` → FAIL.
- [ ] **Step 3: Implement**, extracting `record_swap` first with the existing swap tests as the safety net (`cargo test -p ccr-core swap::` must stay green before adding the relay branch).
- [ ] **Step 4: Run** `cargo test -p ccr-core` → PASS.
- [ ] **Step 5: Commit** `feat(relay): relay-mode swaps and the mode change`.

### Task 6: Status line in relay mode

**Files:**
- Modify: `crates/ccr-core/src/statusline.rs` (`tick`, `sync_profile_login`, `adopt_manual_login`), `crates/ccr-core/src/relay.rs` (bypass bookkeeping)

**Interfaces:**
- Consumes: Task 1.
- Produces: `relay::note_bypass(store, session_id: &str, now_ms: i64)`, `relay::bypass_count(store, now_ms) -> Result<usize>` (meta keys `relay_bypass:<session>`, counted within 10 minutes, pruned by `store.prune`); `StatusInput` keeps its fields; `tick` reads the session's `ANTHROPIC_BASE_URL` from the process environment (the status line inherits it).

In relay mode `tick` records no sample from `rate_limits`, never sets `result.confirm` from attribution, and calls `scheduler::decide(.., thresholds: false)` (triggers 3 and 5 only); it still records model, version and cost (cost on the active account). `sync_profile_login`/`adopt_manual_login` use `relay::profile_account` where they use the active account today. When the environment's `ANTHROPIC_BASE_URL` is not the relay's `base_url`, `note_bypass`.

- [ ] **Step 1: Write failing tests**:
  - `relay_tick_records_no_windows`: mode on, input with `rate_limits` → no new sample; cost noted for the active account.
  - `relay_tick_no_threshold_swap`: active at 99% five_hour in the store → `result.swap` is `None`; a dead home login still yields `DeadLogin`.
  - `relay_writeback_to_home`: mode on, home A, active B, the profile's login renewed (new refresh fp of A) → stored under A, not B.
  - `relay_bypass_counted`: tick with an env lacking the relay URL → `bypass_count == 1`; after 10 minutes → 0.
- [ ] **Step 2: Run** `cargo test -p ccr-core statusline::tests::relay` → FAIL.
- [ ] **Step 3: Implement**; pass the environment lookup into `tick` as a parameter (`env: impl Fn(&str) -> Option<String>`) so tests need not set process variables; the `cc-rotate-ctl statusline` caller passes `std::env::var`.
- [ ] **Step 4: Run** `cargo test -p ccr-core statusline` → PASS.
- [ ] **Step 5: Commit** `feat(relay): status line in relay mode`.

### Task 7: The relay server

**Files:**
- Create: `crates/ccr-relay/Cargo.toml`, `crates/ccr-relay/src/lib.rs`, `crates/ccr-relay/src/login.rs`, `crates/ccr-relay/tests/proxy.rs`
- Modify: workspace `Cargo.toml` (member), `crates/ccr-cli/Cargo.toml` (dependency; feature `relay-test-upstream` forwarding to `ccr-relay/test-upstream`), `crates/ccr-cli/src/main.rs` (hidden `Relay { action }` with `Run`)

**Interfaces:**
- Consumes: Tasks 1, 2.
- Produces:
  - `ccr_relay::Upstream` (`Upstream::anthropic()`; `Upstream::for_test(base: http::Uri)` exists only with feature `test-upstream`)
  - `ccr_relay::serve(listener: std::net::TcpListener, cfg: RelayConfig, upstream: Upstream, logins: Arc<dyn Logins>, events: Arc<dyn RelayEvents>) -> impl Future<Output = Result<()>>`
  - `trait Logins: Send + Sync { fn for_model(&self) -> Result<Choice>; }` with `enum Choice { PassThrough { account: i64 }, Token { account: i64, token: Zeroizing<String> } }`; `login::StoreLogins` implements it from the store (spec 5.1: re-read the active account each call; decrypt only when `credential_meta` changes; `PassThrough` for the home account)
  - `trait RelayEvents: Send + Sync` (no-op default methods; Tasks 8–9 fill them): `response(&self, account: i64, session: Option<&str>, model: Option<&str>, status: u16, headers: &[(String, String)])`
  - `cc-rotate-ctl relay run`: takes `relay.lock` (exit 0 at once if held), binds `127.0.0.1:<port>` from `relay.json`, and serves; with feature `relay-test-upstream` the env var `CC_ROTATE_RELAY_TEST_UPSTREAM` selects the upstream

Implementation choices: hyper 1 server with `hyper_util::server::conn::auto`; client `hyper_util::client::legacy::Client` with `hyper_rustls::HttpsConnectorBuilder::new().with_platform_verifier().https_only().enable_http1().enable_http2()`; request bodies collected (`http_body_util::BodyExt::collect`) before forwarding; responses streamed as `Incoming`; dropping the client side drops the upstream future. Log lines go to `paths.relay_log()` with method, unprefixed path, status, `request-id`, account id.

- [ ] **Step 1: Write failing integration tests** in `crates/ccr-relay/tests/proxy.rs`, against a local hyper fake upstream that records each request's headers (and serves SSE in chunks with delays):
  - `model_request_gets_pool_token`: fake `Logins` → `Token{account:2, "tok-b"}`; a `POST /r/<s>/v1/messages` carrying `Authorization: Bearer home` arrives upstream with `Bearer tok-b`, other headers and body byte-identical, path `/v1/messages?beta=true`.
  - `pass_through_keeps_own_login`: `HEAD /r/<s>/api/hello` and `Choice::PassThrough` model requests arrive with `Bearer home`.
  - `api_key_session_unchanged` (spec 5.1): a model request with `x-api-key` and no `Authorization: Bearer` arrives with its headers unchanged and no pool token.
  - `refused_never_reaches_upstream`: wrong secret / `Origin` / wrong `Host` → 404, upstream saw nothing.
  - `streams_incrementally`: the first SSE chunk reaches the client before the fake sends the last one.
  - `client_disconnect_cancels_upstream`: dropping the client mid-stream ends the upstream request within 1 s.
  - `swap_mid_stream_keeps_stream` (Review Focus): the `Logins` choice changes while a stream is open → that stream completes; the next request uses the new token.
  - `concurrent_requests`: 50 parallel requests all succeed.
- [ ] **Step 2: Run** `cargo test -p ccr-relay` → FAIL (crate empty).
- [ ] **Step 3: Implement** `serve`, `StoreLogins` and `relay run`.
- [ ] **Step 4: Run** `cargo test -p ccr-relay` and `cargo test -p ccr-relay login` (a `StoreLogins` unit test: home active → `PassThrough`; other active → its token; re-decrypts after `save_credential`) → PASS; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] **Step 5: Commit** `feat(relay): the relay server`.

### Task 8: Readings, threshold swaps and resending at a limit

**Files:**
- Create: `crates/ccr-relay/src/events.rs` (`StoreEvents`)
- Modify: `crates/ccr-relay/src/lib.rs` (resend loop), `crates/ccr-relay/tests/proxy.rs`

**Interfaces:**
- Consumes: Tasks 2, 5, 7.
- Produces:
  - `RelayEvents::response` implemented by `StoreEvents`: records `Sample { profile: "relay", session_id: X-Claude-Code-Session-Id, model: body's "model", windows: relay_proto::readings(..) }` for the account used; for the active account, evaluates `scheduler::decide(.., thresholds: true)` and, on a reason, runs `swap::run` in `spawn_blocking` (`wait: false`)
  - `trait RelayEvents` gains `refused_at_limit(&self, account: i64) -> Swapped` (`enum Swapped { To(i64), No }`): runs `swap::run(SwapRequest { from: Some(account), to: None, reason: Reason::LimitRefused, wait: true })` in `spawn_blocking`; a held swap lock is waited for, then the active account is re-read
  - events `relay_resend` (from, to) via `store.log_event`, and `relay_error` (detail: the error's kind, never a value) when Anthropic cannot be reached, at most one a minute; the session gets a 502

Resend loop (spec 8): read status + headers first; if `is_limit_refusal` and nothing sent, call `refused_at_limit`; on `To(id)` with `id` not yet tried for this request, re-run `Logins::for_model` and resend the buffered body; else return the last refusal unchanged. One request tries each account at most once.

- [ ] **Step 1: Write failing tests** (fake upstream answering per token):
  - `records_reading_for_serving_account`: a 200 with the F37 headers → `StoreEvents` records one `relay` sample on the token's account with `five_hour`/`seven_day`.
  - `resend_on_limit`: token B gets the 429 with `rejected`; `refused_at_limit` switches the fake `Logins` to C → client sees C's 200; upstream saw B then C with the same body.
  - `no_swap_returns_refusal`: `refused_at_limit` → `No` → client gets the 429 body and headers unchanged.
  - `each_account_once`: B → C → B refusals → stops after C; the client gets C's refusal.
  - `concurrent_refusals_swap_once` (Review Focus): two requests refused by B at once → `swap::run` called once (a store-level test with `StoreEvents` and a real in-memory pool), both resent on C.
  - `other_errors_pass`: a 529 or a 429 without `rejected` is passed through, no swap.
  - `upstream_down_logs_once`: the fake upstream closed → the client gets 502; two failures within a minute log one `relay_error`.
- [ ] **Step 2: Run** `cargo test -p ccr-relay` → FAIL on the new tests.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(relay): usage readings and resend at a limit`.

### Task 9: Renewal and 401s

**Files:**
- Modify: `crates/ccr-relay/src/login.rs`, `crates/ccr-relay/src/lib.rs`, `crates/ccr-relay/tests/proxy.rs`

**Interfaces:**
- Consumes: Tasks 2, 4, 7, 8.
- Produces: `trait Logins` gains `renew(&self, account: i64)` (start at most one renewal per account: `spawn_blocking(usage_check::renew)`) and `refresh(&self, account: i64) -> Result<Choice>` (re-read the stored login now); `StoreLogins::for_model` calls `renew` when `renew_due(expires_at, now, relay_renew_margin_s)`.

401 path (spec 9): on a 401 to a `Token` request, `refresh`; if the token changed, resend once; otherwise `renew` and poll `refresh` every 500 ms up to 30 s, holding that request; still no new token → mark the account held back (`usage_check` state `held_back = true`) and go through `refused_at_limit`'s swap path (reason `DeadLogin`). A `PassThrough` 401 is passed to the session unchanged (Claude Code renews its own login).

- [ ] **Step 1: Write failing tests**:
  - `renew_started_before_expiry`: stored `expiresAt` = now + 10 min with margin 900 → one `renew(B)` call, the request still sent with the current token.
  - `renew_once_at_a_time`: 20 requests in that window → one renewal.
  - `unauthorized_refreshes_and_resends`: first 401, `refresh` returns a new token → resent once, 200.
  - `unauthorized_holds_then_swaps`: `refresh` never changes within the hold (test hold shortened through a `Duration` field on the relay's options) → account held back, swap path taken, request resent on C.
  - `passthrough_401_unchanged`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test -p ccr-relay` → PASS.
- [ ] **Step 5: Commit** `feat(relay): renew logins through Claude Code`.

### Task 10: Service, lifecycle and the `relay` command

**Files:**
- Create: `crates/ccr-core/src/relay_service.rs`
- Modify: `crates/ccr-core/src/relay.rs` (`turn_on`/`turn_off` call the service), `crates/ccr-cli/src/main.rs` (`relay`, `relay on --home <account>`, `relay off`), `crates/ccr-relay/src/lib.rs` (upgrade watch, idle exit), `crates/ccr-core/src/uninstall.rs`

**Interfaces:**
- Consumes: Tasks 1, 5, 7.
- Produces:
  - `relay_service::launch_agent_plist(ctl: &Path, env: &[(&str, String)]) -> String` (label `cc-rotate.relay`, `ProgramArguments` = `[ctl, "relay", "run"]`, `RunAtLoad` and `KeepAlive` true, `EnvironmentVariables` from `env`), `systemd_unit(ctl: &Path, env: &[(&str, String)]) -> String` (`Restart=always`, `ExecStart=<ctl> relay run`, one `Environment=` per pair), `schtasks_args(ctl: &Path) -> Vec<String>` (at logon, restart on failure); `env` is `CC_ROTATE_DATA_DIR` and `CC_ROTATE_PROFILE` when set where `relay on` runs, so the service serves the same store
  - hidden `relay on --no-service`: everything but the service (for the end-to-end tests, which start `relay run` themselves)
  - `relay_service::install(ctl: &Path) -> Result<()>` / `remove() -> Result<()>`: write the file and run `launchctl bootstrap gui/<uid> <plist>` / `systemctl --user enable --now cc-rotate-relay.service` / `schtasks /Create ...`; removal mirrors them
  - `relay_service::status() -> ServiceState { Installed, Missing }`
  - relay run loop: every 60 s, when `std::env::current_exe()`'s modification time or size changed, stop accepting, wait for in-flight requests to reach zero, exit 0; when `relay::home` is `None` and no request came for 3600 s, `relay_service::remove()`, delete `relay.json`, exit 0
  - `relay` (no action) prints: mode, home label, `127.0.0.1:<port>` (never the secret), answering yes/no, service state, `bypass_count`

`relay on --home <account>` (spec 4): refuse unless the account exists, is not `needs_login`, and its check state is not `held_back`; `RelayConfig::create` when missing; `relay_service::install(current_exe)`; `relay::turn_on`. `relay off`: `relay::turn_off`. `uninstall`: when relay mode is on, `turn_off` first, then `relay_service::remove()` and delete `relay.json`; its shown plan lists those steps.

- [ ] **Step 1: Write failing tests**: `plist_text` (contains the label, the three arguments, `RunAtLoad`, `KeepAlive`, and `CC_ROTATE_DATA_DIR` when passed), `unit_text` (`Restart=always`, the `ExecStart` line, a path with spaces quoted), `on_refuses_signed_out` (Review Focus companion), `uninstall_plan_lists_relay`, and in `ccr-relay`: `exits_after_upgrade_when_idle` (replace a temp file standing in for the executable through a test hook; the serve future resolves only after an open stream ends), `idle_exit_when_off`.
- [ ] **Step 2: Run** `cargo test -p ccr-core relay_service uninstall && cargo test -p ccr-relay` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS. Installing the real LaunchAgent is left to Task 13: a scratch run would replace the user's `cc-rotate.relay` agent.
- [ ] **Step 5: Commit** `feat(relay): service, lifecycle and the relay command`.

### Task 11: Doctor, desktop output and documentation

**Files:**
- Modify: `crates/ccr-core/src/doctor.rs` (new `relay_check`), `crates/ccr-core/src/relay_proto.rs` (`Route::Version`), `crates/ccr-relay/src/lib.rs` (answer it with `env!("CARGO_PKG_VERSION")`), `crates/ccr-cli/src/main.rs` (`desktop` names relay mode and the home account), `README.md`, `docs/specs/2026-10-24-cc-rotate.md` (the edits listed in spec 14), `docs/specs/2026-11-10-relay-mode.md` (status line)

**Interfaces:**
- Consumes: Tasks 1, 6, 10.
- Produces: `fn relay_check(store: &Store, paths: &Paths, answers: impl Fn(u16) -> Option<String>) -> Check` (the closure returns the running relay's version from a `GET /r/<s>/__ccr/version` answered locally by the relay; add that route to `relay_proto::route` as `Route::Version` and to Task 7's server)

Statuses (spec 11): off → Ok "relay mode is off"; on + answering this version → Ok; other version → Warn; not answering → Fail with the last line of `relay.log`; service missing → Fail; home account `needs_login` or missing → Fail "the home account <label> is signed out: every session fails until it signs in again" (Review Focus: `home_signed_out_fails`); the profile's `settings.json` `env` setting `ANTHROPIC_BASE_URL`, `ANTHROPIC_UNIX_SOCKET`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` → Warn naming it; `bypass_count > 0` → Warn with the count.

- [ ] **Step 1: Write failing tests** for each status above: `relay_off_ok`, `relay_ok`, `relay_other_version_warns`, `relay_not_answering_fails`, `relay_service_missing_fails`, `home_signed_out_fails`, `relay_env_override_warns`, `relay_bypass_warns` (all `doctor::tests::`), plus `route_version` in `relay_proto`.
- [ ] **Step 2: Run** `cargo test -p ccr-core doctor::tests::relay relay_proto` → FAIL.
- [ ] **Step 3: Implement**; write the README section ("Relay mode": what it is, `relay on --home <the account the desktop app is signed in to>`, start sessions with `cc-rotate` or from the app, the home account owns artifacts, connectors, cloud work and Remote Control, what is not covered from spec 15) and apply spec 14's edits to the main spec, moving F34–F37 into its findings table.
- [ ] **Step 4: Run** `cargo test --workspace` → PASS; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] **Step 5: Commit** `docs: relay mode in the README and spec; doctor's relay line`.

### Task 12: End-to-end tests

**Files:**
- Create: `tests/e2e/relay.py`
- Modify: `tests/e2e/fake-api.mjs` (log the path prefix it was reached on; nothing else changes)

**Interfaces:**
- Consumes: everything above; the existing staging example `crates/ccr-core/examples/e2e_stage.rs --made-up` and `fake-api.mjs`.

`relay.py` builds `cc-rotate-ctl` and `cc-rotate` with `--features relay-test-upstream`, stages three made-up accounts (A home, B, C) in a scratch store and profile (`CC_ROTATE_DATA_DIR`, `CC_ROTATE_PROFILE`) as `desktop-wait-at-limit.py` does, starts `fake-api.mjs <map> b`, turns relay mode on with `relay on --home a --no-service`, starts `relay run` itself with `CC_ROTATE_RELAY_TEST_UPSTREAM=http://127.0.0.1:<fake port>`, and runs Claude Code headless through the built `cc-rotate` (its `claude_path` set to `--claude <path>`).

Scenarios, each asserting on the fake API's JSON lines (`who`) and the scratch store:
- `swap_retargets`: active B, turn 1 served by B; `cc-rotate-ctl swap --to c`; turn 2 served by C; the profile's credentials file unchanged throughout.
- `resend_at_limit`: B limited → the turn's request is seen by the fake as B (429) then C (200); the turn completes; no `StopFailure` event and no waiting session in the store.
- `relay_restart`: kill the relay between turns; the next turn succeeds (the launcher's fallback starts it — start the next turn through `cc-rotate`, which probes and spawns; `CC_ROTATE_RELAY_TEST_UPSTREAM` is in the launcher's environment, so the relay it starts reaches the fake API).
- `relay_down`: relay killed and its program path made unrunnable → the turn reports a connection error (`Unable to connect to API`), and `doctor --json` shows the Relay line `fail`.
- `version_check`: with the relay's two variables, `claude -p --debug-file <f>` logs in with the stored login (no "Not logged in"), its model requests reach the relay (fake API lines arrive via the relay's prefix), and `<f>` has no line containing `custom_base_url`. If Claude Code logs nothing about the first-party check either way, the script prints that the check is unverified, and Task 13's model-picker step decides.

Renewal is not covered here: the fake API cannot renew a login. Task 9's tests cover it, and the in-app check below exercises it over a day.

- [ ] **Step 1: Write** `tests/e2e/relay.py` with the five scenarios.
- [ ] **Step 2: Run** `python3 tests/e2e/relay.py --claude "$(which claude)"` → expect PASS on all five (it fails first if any earlier task is incomplete).
- [ ] **Step 3: Fix** whatever it finds in the owning task's code, with a unit test for each fix.
- [ ] **Step 4: Run** again → PASS; also `python3 tests/e2e/desktop-wait-at-limit.py` → PASS (relay mode off, unchanged).
- [ ] **Step 5: Commit** `test(e2e): relay mode`.

### Task 13: In the app (manual, macOS)

Not code: the acceptance check from spec 16, run by the user or with them, after Task 12 passes.

- [ ] Install the build; `cc-rotate-ctl relay on --home <the app's account>`; quit and reopen the desktop app.
- [ ] A new Code session answers; `relay.log` shows its model requests on the active account.
- [ ] The session's model picker offers the same models as with relay mode off (first-party treatment, spec F35).
- [ ] `cc-rotate-ctl swap --to <another account>`; the session's next turn is served by it (`relay.log`).
- [ ] Publish an artifact in that session; it opens in the app's artifact pane.
- [ ] Leave it a working day; `cc-rotate-ctl events` shows renewals (`sync` with reason `usage_check`) of the non-home account before its expiry and no 401s in `relay.log`.
- [ ] Record the results in spec 16 and the main spec's 24.6.
