"""Live end-to-end check: a session stopped at a usage limit continues once cc-rotate swaps.

Runs the real, interactive Claude Code in a pseudo-terminal against a fake Messages API
(fake-api.mjs), in a scratch cc-rotate store and profile that this checkout's own cc-rotate-ctl
sets up, so its status line and StopFailure hook are the ones under test (spec section 21).

Two accounts from the real store are copied in with dummy refresh tokens (examples/e2e_stage.rs),
so nothing in the test can renew, and so retire, a real login. Their access tokens reach Anthropic
only for Claude Code's own account lookups; every model request goes to the fake API, which
answers the first account with a usage-limit 429 and the second normally, so no quota is used.
The installed cc-rotate, its store and its profile are not touched.

Passes when the session stops at the first account's limit, the hook asks for a swap, wakes the
session once the second account is in the profile, and the session continues on it.

With --closed, both accounts are at their limit: passes when the session stays stopped, and the
hook stops waiting once the session is closed.

Usage (each account needs at least 20 minutes left on its access token; the stager checks):
  Windows:      uv run --with pywinpty --with pyte tests/e2e/wait-at-limit.py <labelA> <labelB> [--closed]
  macOS/Linux:  uv run --with ptyprocess --with pyte tests/e2e/wait-at-limit.py <labelA> <labelB> [--closed]
"""

import atexit
import json
import os
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time

import pyte

if os.name == "nt":
    from winpty import PtyProcess
else:
    from ptyprocess import PtyProcessUnicode as PtyProcess

sys.stdout.reconfigure(encoding="utf-8")
args = [a for a in sys.argv[1:] if a != "--closed"]
closed = "--closed" in sys.argv[1:]
if len(args) != 2:
    sys.exit(__doc__)
label_a, label_b = args
repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
t0 = time.time()


def log(*parts):
    print(f"[{time.time() - t0:6.1f}s]", *parts, flush=True)


# ---- scratch store and profile ----------------------------------------------------------------

children = []
scratch = tempfile.mkdtemp(prefix="cc-rotate-e2e-wait-")


def cleanup():
    for child in children:
        try:
            child.terminate(True) if isinstance(child, PtyProcess) else child.kill()
        except Exception:
            pass
    # The scratch store holds copies of access tokens: never leave it behind. Windows can hold the
    # folders a little longer after Claude Code is stopped.
    def writable(func, path, _):
        # A read-only file stops rmtree on Windows.
        os.chmod(path, stat.S_IWRITE)
        func(path)

    error = None
    for _ in range(240):
        try:
            # `onexc` from Python 3.12, `onerror` before it; the handler takes either.
            shutil.rmtree(scratch, **({"onexc": writable} if sys.version_info >= (3, 12) else {"onerror": writable}))
            return
        except FileNotFoundError:
            return
        except OSError as e:
            error = e
            time.sleep(0.5)
    print(f"Could not delete {scratch} ({error}); delete it by hand.", flush=True)


atexit.register(cleanup)

exe = ".exe" if os.name == "nt" else ""
subprocess.run(["cargo", "build", "-q", "-p", "ccr-cli"], cwd=repo, check=True)
subprocess.run(["cargo", "build", "-q", "-p", "ccr-core", "--example", "e2e_stage"], cwd=repo, check=True)
target = os.environ.get("CARGO_TARGET_DIR") or os.path.join(repo, "target")
ctl_exe = os.path.join(target, "debug", f"cc-rotate-ctl{exe}")
stager = os.path.join(target, "debug", "examples", f"e2e_stage{exe}")

data, profile, template, work = (os.path.join(scratch, d) for d in ("data", "profile", "template", "work"))
token_map = os.path.join(scratch, "tokens.json")
for d in (template, work):
    os.makedirs(d)
with open(os.path.join(template, "settings.json"), "w") as f:
    f.write("{}")

real_env = {k: v for k, v in os.environ.items() if k not in ("CC_ROTATE_DATA_DIR", "CC_ROTATE_PROFILE", "CLAUDE_CONFIG_DIR")}
log(subprocess.run([stager, data, profile, token_map, label_a, label_b], env=real_env, capture_output=True, text=True, check=True).stdout.strip())
ctl_env = {**real_env, "CC_ROTATE_DATA_DIR": data, "CC_ROTATE_PROFILE": profile}


def ctl(*args):
    out = subprocess.run([ctl_exe, *args], env=ctl_env, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(f"cc-rotate-ctl {' '.join(args)} failed: {out.stderr.strip()}")
    return out.stdout.strip()


ctl("setup", "--from", template)
log(ctl("swap", "--to", label_a))
config_file = os.path.join(profile, ".claude.json")
config = json.load(open(config_file, encoding="utf-8")) if os.path.exists(config_file) else {}
config.update(
    hasCompletedOnboarding=True,
    numStartups=5,
    theme="dark",
    projects={work.replace("\\", "/"): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}},
)
json.dump(config, open(config_file, "w", encoding="utf-8"))

# ---- the fake API ------------------------------------------------------------------------------

requests, lock = [], threading.Lock()
server = subprocess.Popen(
    ["node", os.path.join(repo, "tests", "e2e", "fake-api.mjs"), token_map, "*" if closed else label_a],
    stdout=subprocess.PIPE,
    text=True,
)
children.append(server)
port = json.loads(server.stdout.readline())["port"]


def read_server():
    for line in server.stdout:
        request = json.loads(line)
        with lock:
            requests.append(request)
        if request.get("path") == "/v1/messages":
            log(f"model request as {request['who']}: {request['status']}")


threading.Thread(target=read_server, daemon=True).start()


def first(match):
    with lock:
        return next((r for r in requests if match(r)), None)


def wait_for(match, seconds):
    deadline = time.time() + seconds
    while time.time() < deadline:
        found = first(match)
        if found:
            return found
        time.sleep(0.25)
    return None


# ---- Claude Code, interactive ------------------------------------------------------------------

env = {k: v for k, v in real_env.items() if not k.startswith(("ANTHROPIC_", "CLAUDE_CODE_", "CLAUDECODE", "HERDR_"))}
env.update(
    CLAUDE_CONFIG_DIR=profile,
    CC_ROTATE_DATA_DIR=data,
    CC_ROTATE_PROFILE=profile,
    ANTHROPIC_BASE_URL=f"http://127.0.0.1:{port}",
    DISABLE_AUTOUPDATER="1",
    DISABLE_TELEMETRY="1",
    DISABLE_ERROR_REPORTING="1",
)
cols, rows = 120, 40
screen = pyte.Screen(cols, rows)
stream = pyte.Stream(screen)
claude = PtyProcess.spawn([shutil.which("claude"), "--model", "haiku", "say pong"], cwd=work, env=env, dimensions=(rows, cols))
children.append(claude)


def read_pty():
    while claude.isalive():
        try:
            stream.feed(claude.read(4096))
        except EOFError:
            break


threading.Thread(target=read_pty, daemon=True).start()


def show_screen():
    print("\n".join(line.rstrip() for line in screen.display).strip("\n"), flush=True)


failures = []


def waiting_state():
    db = sqlite3.connect(f"file:{os.path.join(data, 'store.db')}?mode=ro", uri=True)
    try:
        row = db.execute("SELECT state FROM waiting_sessions ORDER BY since_ms DESC LIMIT 1").fetchone()
        return row[0] if row else None
    finally:
        db.close()


if closed:
    stopped = wait_for(lambda r: r.get("status") == 429 and r.get("stream"), 120)
    if not stopped:
        sys.exit("FAIL: the session never stopped at a limit")
    log("the session stopped at a limit, with no account left that has room")
    time.sleep(20)
    if first(lambda r: r.get("status") == 200 and r.get("stream")):
        failures.append("the session was woken although no account had room")
    if waiting_state() != "waiting":
        failures.append(f"the stop is {waiting_state()!r}, not waiting")
    claude.terminate(True)
    log("closed the session")
    deadline = time.time() + 20
    while time.time() < deadline and waiting_state() == "waiting":
        time.sleep(0.5)
    if waiting_state() != "ended":
        failures.append(f"the hook kept waiting after the session closed (state {waiting_state()!r})")
    for failure in failures:
        log("FAIL:", failure)
    if failures:
        sys.exit(1)
    log("PASS")
    sys.exit(0)

limited = wait_for(lambda r: r.get("who") == label_a and r.get("status") == 429 and r.get("stream"), 120)
if not limited:
    failures.append(f"the session never sent a request as {label_a}")
else:
    log(f"the session stopped at {label_a}'s limit")
    resumed = wait_for(lambda r: r.get("who") == label_b and r.get("status") == 200 and r.get("stream"), 180)
    if resumed:
        log(f"the session continued on {label_b} {(resumed['t'] - limited['t']) / 1000:.1f}s after it stopped")
    else:
        failures.append(f"the session did not continue on {label_b} within 3 minutes")

# The status line reports the woken session's usage on its new account.
if not failures:
    deadline = time.time() + 60
    while time.time() < deadline:
        events = json.loads(ctl("events", "--json", "--limit", "50"))
        reasons = {(e["kind"], e.get("reason")) for e in events}
        if ("session", "resumed") in reasons:
            break
        time.sleep(1)
    for expected in [("session", "waiting"), ("swap", "stop_failure"), ("session", "woken"), ("session", "resumed")]:
        if expected not in reasons:
            failures.append(f"no {expected[0]} event with reason {expected[1]}")

time.sleep(2)
show_screen()
db = sqlite3.connect(f"file:{os.path.join(data, 'store.db')}?mode=ro", uri=True)
for ts, label, name, pct, session in db.execute(
    "SELECT s.ts, a.label, w.name, w.pct, s.session_id FROM samples s JOIN accounts a ON a.id = s.account_id "
    "JOIN sample_windows w ON w.sample_id = s.id ORDER BY s.ts"
):
    log(f"reading at {(ts / 1000 - t0):5.1f}s: {label} {name} {pct:.0f}% (session {str(session)[:8]})")
db.close()
if failures:
    for failure in failures:
        log("FAIL:", failure)
    sys.exit(1)
log("PASS")
