# cc-rotate relay mode: every session on the app's account, model requests on the pool

Status: built, 2026-11-10; this text follows what was built.
The end-to-end tests and the check in the app (section 16) are still to run.
Builds on the main spec (`2026-10-24-cc-rotate.md`), whose section numbers are cited as "main 24.1" and so on.

## 1. Goal

Artifacts published in any desktop Code session open in the Claude desktop app's artifact pane, whichever pool account was in use when they were published (main 24.5, F32).

An artifact belongs to the account whose login published it, and the app's pane opens it with the app's own sign-in.
So every session has to publish with the app's account, while its model requests keep running on the pool.
Relay mode does that: the cc-rotate profile keeps the app's account's login for good, and a local relay sends each session's model requests to Anthropic with the login of the pool account in use.
A swap changes which login the relay attaches, instead of writing the profile's login.

Relay mode is off by default and covers the whole profile, terminal and desktop sessions alike: they share one stored login (main 18), so the profile's login cannot be the app's account for desktop sessions only.
With it off, everything works as in the main spec.

## 2. Findings

Findings F34 to F37 are in the main spec's findings table (main 3), where they were seen with Claude Code 2.1.322 on macOS, with real accounts.
What each one means here:

- F34: Anthropic serves and counts a model request on the account whose login it carries, whatever account its body names, so the relay leaves the body alone and only swaps the login (section 5).
- F35: `ANTHROPIC_BASE_URL` with a path keeps a session on the profile's claude.ai login and sends only model requests there, which is how sessions reach the relay (section 4); `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` is internal to Claude Code, so it is checked on every new version (section 16).
- F36: `ANTHROPIC_UNIX_SOCKET` signs a session out, so the relay listens on loopback TCP, not on a socket file.
- F37: every model response carries the serving account's usage, so the relay reads usage for the account it used, desktop sessions included (section 7).

## 3. Overview

- The **home account** is the pool account the desktop app is signed in to.
  The user names it when turning relay mode on; cc-rotate cannot see the app's account, since the app sends no login in launcher mode (main F27).
- The cc-rotate profile's login is the home account's for as long as relay mode is on.
  Claude Code renews it in the profile, and the status line's write-back stores it as today (main 8, step 3).
- The **active** account is the one the relay sends model requests on.
  Swaps, thresholds and the pick work as before, but a swap only changes which account is active (section 6).
- Every session in the profile reaches the relay through its environment (section 4).
  Everything that is not a model request still goes straight to Anthropic on the home account's login: artifacts, connectors, cloud work, Remote Control.

## 4. Turning it on and off, and what sessions get

`cc-rotate-ctl relay` shows whether relay mode is on, the home account, the relay's address and whether it answers.

`cc-rotate-ctl relay on --home <account>`:

1. Refuses unless the account is in the pool, signed in, and not held back by a check.
2. Picks a free port on 127.0.0.1 between 49152 and 65535 and a random 256-bit secret, and writes them to `<data dir>/relay.json` (mode 0600; the data folder is 0700).
   Both stay the same until relay mode is turned off, so sessions started earlier keep reaching a restarted relay.
3. Installs the relay's service (section 10) and starts it.
4. Under the swap lock, makes the home account's login the profile's and turns the mode on, without changing which account is active.
   When another account is active, its login is written out of the profile and the home account's in, with the main spec's locks and steps (main 6, steps 4 to 6 and 8), and the transaction that would set the states (main 6, step 7) sets `relay_mode` to `on` and `home_account` (section 12) instead; when the home account is active, that transaction is all there is.
   So the status line's write-back never finds the home account's login in the profile while relay mode is off.
   It is refused while a swap is pending (main 6, step 4).
   The account active before stays active, now through the relay.

From then on both launchers, `cc-rotate` (main 4.4) and `cc-rotate-ctl desktop-launch` for a desktop session (main 24.1), add two variables to the session's environment:

- `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/r/<secret>`
- `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1`

They read only `relay.json` and the `relay_mode` setting, read-only, within the launcher's time budget.
They do not add them to a desktop session the launcher keeps in the app's own profile (a resumed older session, main 24.1 step 3), which is not in the cc-rotate profile.
Claude Code's own subprocesses (subagents, teammates, its background service, a `claude` the agent runs) inherit them.
A `claude` started in the profile some other way (plain `claude` with `CLAUDE_CONFIG_DIR` set) bypasses the relay and runs on the home account directly.

`cc-rotate-ctl relay off`:

1. Under the swap lock, when the active account is not the home account, writes the active account's login into the profile with the main spec's locks and steps (main 6, steps 4 to 6 and 8, the home account's login saved to its row first), so the main spec's invariant holds again (main 5.1).
   The transaction that commits it sets `relay_mode` to `off`; when the home account is active, that transaction is all there is.
   New sessions then start without the variables.
2. Leaves the relay running for sessions started with it; after an hour without requests it deletes `relay.json`, removes its service and exits (section 10).
   Meanwhile the relay only forwards: it records no reading, asks for no swap, holds no account back and starts no renewal, and a refusal goes back to the session as it is.
   The status line records readings and asks for swaps again, and a swap writes the profile's login, which a session started with the relay does not send.
3. Says that the sessions started while relay mode was on must be restarted within the hour: once the relay has left, they get connection errors.

Sessions running when relay mode changes keep the environment they started with until they are restarted.
Turning it on leaves earlier sessions on the profile's login, now the home account's, off the pool.
The status line runs with its session's environment, so it sees whether the session has the relay's address; `relay` and `doctor` count the sessions without it among those whose status line reported in the last 10 minutes (desktop sessions run none, main F30, and start with it once relay mode is on).
With relay mode off, the status line records a session whose `ANTHROPIC_BASE_URL` is a relay's address (`http://127.0.0.1:<port>/r/...`), and `doctor` warns while any reported in the last 10 minutes: it was started while relay mode was on and has to be restarted.

## 5. The relay

`cc-rotate-ctl relay run` (hidden) is a reverse proxy from `127.0.0.1:<port>` to `https://api.anthropic.com`.

For each request:

1. It refuses, with a bare 404 and a log line without the request's details, a request whose path does not start with `/r/<secret>/`, whose `Host` is not `127.0.0.1:<port>`, that carries an `Origin` header, or whose method is not `GET`, `HEAD` or `POST`.
2. It strips the prefix and decides by the path:
   - `POST /v1/messages` and `POST /v1/messages/count_tokens` (any query) are **model requests**: the incoming `Authorization` is replaced with `Bearer <access token>` of the active account (section 5.1).
   - Anything else is forwarded with the session's own headers unchanged; the relay attaches no pool login to it.
3. It forwards the request to `https://api.anthropic.com` with every header unchanged except `Authorization` (model requests only), `Host`, and the hop-by-hop headers, and the body unchanged (F34).
   The upstream address is fixed in the program; nothing in a request, the environment or the settings changes it.
   TLS is verified against the system's roots, and redirects are not followed.
4. It reads the response's status and headers before sending anything back, records usage from them (section 7), and decides whether to resend (section 8).
5. Otherwise it streams the response to the session as it arrives (server-sent events included), and stops the upstream request when the session disconnects.

It buffers each request's body, which a resend needs, and imposes no time limit of its own on a response; Claude Code applies its own.
Many sessions and their subagents send at once, so requests are handled concurrently, with connections to Anthropic kept alive and reused.

### 5.1 Which login

- When the active account is the home account, a model request keeps the session's own `Authorization`: it is the profile's login, the home account's, renewed by Claude Code.
- Otherwise the relay attaches the active account's stored access token (main 5, `credentials`), decrypted when the stored login changes (its `refresh_fp` or `updated_at`) and held in memory, zeroed when replaced.
  It reads which account is active from the store on every model request, so a swap reaches the next request of every session.
- A session whose own `Authorization` is not a bearer (an API key) is forwarded unchanged: relay mode does not apply to it, and `doctor` says so (section 11).

## 6. Swaps in relay mode

Triggers, the pick, the cooldown and the checks before a swap are the main spec's (main 6, steps 1 to 3; main 7).
Steps 4 to 8 become:

4. Make sure the target's access token is valid for at least `relay_renew_margin_s` (section 9): if not, renew it with a check first, within the swap's 25 s budget for checks; a target that cannot be renewed is passed over like a busy one.
5. In one transaction, set the outgoing account to `parked` (`needs_login` for a dead-login swap, trigger 3, as in main 6 step 7) and the target to `active`, record the outgoing account as left at a limit for a `rate_limit` swap (section 8) as for a `stop_failure` one (main 7.2), and log the `swap` event as before.
6. Release the swap lock.

No login is written into the profile, Claude Code's locks are not taken, and no swap is ever pending (main 6, step 4), so `settle-login` has nothing to settle in relay mode.
Requests already sent finish on the account they went out on; every request sent after the transaction uses the target.

Swapping to the home account works the same way: the relay then passes the session's own login through (section 5.1).

## 7. Usage readings

After each model response, the relay records a sample (`profile = relay`) for the account it attached, with:

- one window per `anthropic-ratelimit-unified-<window>-utilization` header (F37), the percentage being the fraction times 100, and the reset time from the matching `-reset` header;
- the window named as Claude Code's rate-limit tracker names it (`5h` as `five_hour`, `7d` as `seven_day`, the others by the same mapping, read from the Claude Code bundle when implementing), and an unknown window under the header's own name;
- the session id from `X-Claude-Code-Session-Id` and the model from the request body's `model`.

These readings are certain: the relay knows which account served each one.
In relay mode the status line therefore records no usage windows (main 8, step 2): after a swap a session's own tracker mixes accounts, since its login, and with it Claude Code's account epoch (main F5), never changes.
The status line still records the model, cost, project and Claude Code version (main 22.1), and still does the write-back (main 8, step 3) for the home account's login.
The threshold trigger moves to the relay: after recording a sample of the active account, the relay decides as the status line would (main 6 and 7: the thresholds, and the proactive swaps, switching back and a weekly budget that would reset unused), at most once every 30 s, and runs the swap in its own process, in the background, as `cc-rotate-ctl swap` would.
The status line still evaluates triggers 3 and 5 (main 6).
The confirmation of the account in use (main 20.4) is not needed in relay mode, since every reading names its account.

Desktop sessions, which run no status line (main F30), are read like any other, so they swap before a limit too.

## 8. Resending at a limit

When Anthropic refuses a model request at a usage limit, a 429 with `anthropic-ratelimit-unified-status: rejected`, and nothing of the response has been sent to the session yet:

1. The relay records the reading (section 7).
2. It runs the swap itself (`--from` the refused account, reason `rate_limit`) and waits for it, up to the swap's own budget; when another swap holds the swap lock, it waits for that one instead and then reads which account is active.
   Like a swap at a stopped turn (main 7.2), it records the refused account as left at a limit (`left_at_stop`), so its room is not trusted for a switch back until a newer reading says so.
3. When another account became active, it sends the same request again, with that account's login.
4. It repeats this at most once per pool account for one request.
   When no swap happens (the pool is used up, or the swap was skipped), it returns the last refusal unchanged, and Claude Code stops the turn as before.

A turn whose request was resent never stops, so the `StopFailure` hook (main 21) does not fire for it.
The hook stays for refusals the relay returns, where it waits for a reset as before (main 21.2).
The 31 s wake delay (main 24.3) does not apply in relay mode, since no Keychain item is written.

Any other error, and any refusal once the response has started streaming, is passed to the session unchanged.

## 9. Keeping logins valid

Claude Code renews logins; cc-rotate never calls Anthropic's sign-in endpoints itself (main 2).

- **The home account**: its login is the profile's, and the sessions' Claude Code renews it as today.
  If it dies (main F8), every session fails, relay or not, so `login_warning_days` (main 21) matters for it most; `relay` and `doctor` name the home account in that warning.
- **The active account, when it is not the home account**: no Claude Code holds its login, so the relay asks for a renewal `relay_renew_margin_s` before its access token expires.
  A renewal is a usage check (main 20.1) that sets the copied login's `expiresAt` to 0 so Claude Code renews it at once (as for a held-back account, main 20.1 step 3), and stores the renewed login in the same transaction.
  The relay runs it in its own process, in the background (`usage_check::renew`), at most one at a time per account.
  A renewal that ordinary requests started is not started again within 60 s of the last one ending, so a renewal that fails fast does not run Claude Code on every request; one a 401 asks for (below) is not held back by that.
  The old access token keeps working until it expires, so requests are not held.
- **A 401 on a model request**: the relay reads the stored login again and resends once if it changed; otherwise it asks for a renewal and holds the refused request for up to 30 s; other requests, of that account or another, go on meanwhile.
  If no valid login comes, the account is held back (main 20.3) until a check settles whether its login works, and the relay swaps away from it as in section 8, with reason `unauthorized`; it is not marked `needs_login`.
  A login Anthropic rejects outright is marked `needs_login` by the check, as today.
- **Parked accounts** are renewed only when they become active (section 6, step 4), as today.

In relay mode a check skips the home account instead of the active one (main 20.1, step 2): the home account's stored login is the profile's, which a renewal in a scratch profile would retire.

The profile's login is the home account's, so the status line's write-back (main 8, step 3) watches it as the home account's:

- A dead login in the profile (main F8) marks the home account `needs_login` when the home account is not active, with no swap: a swap cannot fix the login every session needs.
  When the home account is active, the dead-login swap (trigger 3) runs as before.
- A `/login` of another pool account in the profile makes that account the home account.
- A live login of a home account marked `needs_login` (signing it in again) puts it back to `parked`.
The active account, when it is not the home account, is checked like a parked one.
Checks remove every `ANTHROPIC_*` variable (main 20.1, step 4), so they never go through the relay.

## 10. Lifecycle

The relay runs as a per-user service that restarts it when it ends:

- macOS: a LaunchAgent, `~/Library/LaunchAgents/cc-rotate.relay.plist`, with `RunAtLoad` and `KeepAlive`.
- Linux: a systemd user unit, `cc-rotate-relay.service`, with `Restart=always`.
- Windows: a Task Scheduler task at logon that restarts on failure (not run yet, like the rest of Windows desktop support, main 24.4).

Only one relay runs: it holds an OS file lock on `<data dir>/relay.lock`, which the system releases when the process ends however it ends, and binds the fixed port.
A relay a launcher started exits quietly when another relay holds the lock; one the service started (its definition sets `CC_ROTATE_RELAY_SERVICE=1`) waits for the lock instead, so the service does not restart it over and over.
A relay that finds the port taken (`AddrInUse`) exits quietly too, whichever started it, after writing one line to `relay.log`: `cannot start: 127.0.0.1:<port> is taken by another program`.
A relay that cannot start (the store, the profile or the port cannot be opened) writes one line to `relay.log`, waits 10 s and exits with an error, so the service restarts it at that pace.
A launcher that finds nothing answering on the port starts `cc-rotate-ctl relay run` detached and starts Claude Code without waiting, keeping its time budget (main 24.1); the session's first request a moment later finds it running.

Every minute the relay checks whether its program file was replaced (an upgrade).
If so, it keeps serving, and exits the moment no request is in flight, closing kept-alive connections gracefully, at most 660 s after it noticed; the service starts the new program.
Claude Code retries a refused connection (up to 10 times with backoff), so the gap goes unnoticed.

`relay off` (section 4) leaves the relay serving until it has had no request for an hour; it then deletes `relay.json`, removes its service (which stops it) and exits.
`uninstall` stops the relay, removes its service and `relay.json`, and turns relay mode off first, as in section 4.

## 11. When the relay is down

Sessions fail visibly; they never fall back to another way of reaching Anthropic.

- A running session gets connection errors, which Claude Code retries and then reports ("Unable to connect to API").
  The service normally restarts the relay within seconds, inside those retries.
- A new session gets the relay's address whether or not it answers; the launcher has started it (section 10).
- `doctor` gains a "Relay" line: off (ok; warn while sessions started in relay mode still report with a relay's address, section 4); on and answering with this program's version (ok); on and answering with another version (warn); on and not answering, with the last line of its log (fail); the service missing (fail); the home account signed out or gone (fail: every session fails until it signs in again).
  It asks the relay its version with `GET /r/<secret>/__ccr/version`, which the relay answers itself, as plain text, asking Anthropic nothing and attaching no login; any other path, method or query under `__ccr` is forwarded like any request.
  It also warns when the profile's `env` sets `ANTHROPIC_BASE_URL`, `ANTHROPIC_UNIX_SOCKET`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN`, any of which would take a session off the relay or off the home account's login, and when sessions ran without the relay's address in the last 10 minutes (section 4).
- `cc-rotate-ctl desktop` says whether relay mode is on and names the home account.

## 12. Settings and data

| Key | Default |
|---|---|
| `relay_mode` | `off` |
| `home_account` | none; set by `relay on` |
| `relay_renew_margin_s` | 900 (60 to 3600) |

`<data dir>/relay.json` holds `{port, secret}`; `<data dir>/relay.lock` is held by the running relay.
Samples the relay records have `profile = relay`.
Events gain `relay_mode` (relay mode turned on, naming the home account, or off; with the accounts whose login the profile had and has), `relay_resend` (a request resent on another account, with both accounts) and `relay_error` (the relay could not reach Anthropic, at most one a minute).
Swaps gain the reasons `rate_limit` (a refusal at a limit, section 8) and `unauthorized` (a refused login that could not be renewed, section 9).

## 13. Security

The relay can attach any pool login to a request, so it accepts requests only from the sessions it serves.

- It listens on `127.0.0.1` only, never on another address or `::`.
- Every request must carry the 256-bit secret in its path (section 5, step 1); the secret is in `relay.json` (0600, in the 0700 data folder) and in the environment of the sessions, which only the same user can read.
- It refuses a `Host` other than its own address (DNS rebinding) and any request with an `Origin` header (a web page; Claude Code sends none).
- It attaches a pool login only to the two model paths, sends it only to `https://api.anthropic.com` with TLS verified, and never follows a redirect.
- It never logs a token, a request or response body, or a request header's value; its log has the method, path without the prefix, status, request id, account, and the usage headers.
- Pool logins are decrypted only for the active account, held in memory, and zeroed when replaced.

The threat model is the main spec's (main 5.1): other users on the machine and web pages are kept out; code running as the same user can read `master.key` and the profile's login anyway, so the relay gives it nothing new.
The secret also appears in the sessions' environment, which the user's own processes can read (`ps -E` on macOS shows it to the same user only).

One risk remains: the port is fixed, and while no relay listens on it (between an exit and the restart, or after the relay has left while sessions still point at it), another local user can bind it.
That program then receives the sessions' requests, including the path's secret and the home account's access token, which every session sends in its `Authorization` header.
cc-rotate cannot prevent this on a shared machine; a relay that finds its port taken writes so to `relay.log`, and `doctor` shows that line while the relay does not answer.

## 14. Changes to the main spec

All of these are made in the main spec.

- Main 2 (non-goals): "no HTTP proxy, no `ANTHROPIC_BASE_URL` override" and "cc-rotate never calls Anthropic APIs itself" gain an exception: in relay mode a local relay forwards Claude Code's own requests, and resends a request Anthropic refused at a limit. It still starts no request of its own, and logins are still renewed by Claude Code.
- Main 3: findings F34 to F37 (section 2).
- Main 5.1, invariant: in relay mode the home account's newest login lives in the profile, and every other account's in the store.
- Main 6: relay-mode swaps (section 6).
- Main 8: the status line records no usage windows in relay mode, and evaluates only triggers 3 and 5 (section 7); its write-back (step 3) compares the profile's login with the home account's row, not the active one's, and stores renewals there.
- Main 12: `doctor`'s Relay line (section 11).
- Main 13: the settings in section 12.
- Main 20.1, step 2 and 20.4: which account a check skips, and no confirmation in relay mode (sections 7 and 9).
- Main 21 and 24.3: the hook is a backstop, and the 31 s wake delay does not apply (section 8).
- Main 24.1: `desktop-launch` adds the relay's variables (section 4).
- Main 24.5: the artifact pane shows every desktop session's artifacts in relay mode, when the home account is the app's account; the limitation stays for relay mode off.
- Main F18, F19 and the README's "What it does not cover": in relay mode the login sessions hold never changes, so cloud work and Remote Control stay on the home account across swaps, running on its quota.
- The README: what relay mode is, that the home account must be the account the desktop app is signed in to, that sessions must be started with `cc-rotate` or from the app to use it, and that its claude.ai connectors, artifacts, cloud work and Remote Control are the home account's.

## 15. Not covered

- Sessions started without a launcher (section 4), and sessions that were running when relay mode changed, until restarted.
- A machine that needs an HTTP proxy or its own certificate authority to reach Anthropic: the relay connects directly with the system's roots.
- Models or features the home account offers but the active account's plan does not: Claude Code decides what to offer by the home account, so such a request fails on the active account with Anthropic's error.
- Artifacts published before relay mode was on stay with the account that published them.
- Windows, until the tests in section 16 pass there (main 24.4).
- `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` is internal to Claude Code (F35). If a version stops honoring it, sessions on the relay lose first-party behaviour (fewer models offered, no experimental betas) but keep working; the version check in section 16 catches it, and `doctor` warns on an untested version as before.

## 16. Testing

- Unit tests:
  - the request decision as a function of method, path, `Host` and `Origin`: refused, model request, or passed through;
  - the forwarded headers: only `Authorization` replaced, and only on the two model paths;
  - which login: home passes the session's own through, any other account its stored token;
  - usage from response headers, including an unknown window name and a missing reset;
  - the limit refusal (429 with `rejected`) against other 429s and errors, and the resend policy: never after the first byte, at most once per account, the last refusal returned when no swap happens;
  - the renewal schedule from `expiresAt` and `relay_renew_margin_s`, the 60 s wait between renewals, the 401 path, and a renewal storing the renewed login;
  - the launcher's environment plan with relay mode on and off, for both launchers, and for a resumed older desktop session;
  - `relay on`/`off`: refusals, `relay.json`, and the profile's login written only when the active account is not the home account.
- Integration tests: the relay serves a local fake upstream (the upstream is a parameter of the relay's code, fixed to `https://api.anthropic.com` in the program; no flag, variable or setting changes it), with concurrent streamed responses, a session that disconnects mid-stream, a resend, and the refusal rules.
- `tests/e2e/relay.py` runs Claude Code headless through the relay against the fake Messages API, with three accounts staged (main 21.4), built with a test-only feature that points the relay at the fake API:
  - a swap mid-session moves the next request to the target, with the profile's login unchanged;
  - a refusal at a limit is resent on the next account, and the turn finishes without `StopFailure`;
  - the relay killed mid-session is restarted by the launcher's fallback, and the session's next turn works;
  - the relay down: the session reports a connection error.

  Renewal is covered by the unit tests only: the fake API cannot renew a login.
- A version check, run like the other end-to-end tests on each new Claude Code: with the relay's two variables, Claude Code logs in with the stored login, sends model requests to the base URL, and treats it as first-party (it offers the same models as without the relay).
- In the app on macOS, with relay mode on and the home account the app's: a new Code session runs, a swap to another account moves its next turn there (the relay's log), an artifact published while that account is active opens in the artifact pane, and a session at a limit continues on the next account without stopping. Then on Windows.

## 17. Decisions

- Relay mode is opt-in and covers the whole profile (section 1); a separate profile for desktop sessions would cost them seeing the other sessions, and making it the only mode would make every user depend on a running relay.
- Loopback TCP with a path secret, not a socket file (F36), and not a TLS-terminating relay with its own certificate authority.
- The body is forwarded unchanged (F34).
- Logins are renewed by Claude Code through checks, not by cc-rotate calling the token endpoint (section 9).
- A relay that is down fails visibly; sessions do not fall back to the home account (section 11).
- The relay records usage and resends at a limit (sections 7 and 8).
