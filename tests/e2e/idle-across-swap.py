"""Live end-to-end check: a session idle since a swap is not taken for the account in use.

Runs two real, interactive Claude Code sessions in pseudo-terminals against a fake Messages API
(fake-api.mjs, where each account has its own 5-hour numbers), in a scratch cc-rotate store and
profile that this checkout's own cc-rotate-ctl sets up, so its status line is the one under test
(spec section 8, F24).

Two accounts from the real store are copied in with dummy refresh tokens (examples/e2e_stage.rs),
so nothing in the test can renew, and so retire, a real login. Their access tokens reach Anthropic
only for Claude Code's own account lookups; every model request goes to the fake API, so no quota
is used. The installed cc-rotate, its store and its profile are not touched.

Both sessions answer once on the first account, then cc-rotate swaps to the second while they
idle. Their status lines keep reporting the first account's numbers. Well past the two minutes
after a swap in which the first account's reset times are dropped anyway, one session sends
another message, which goes out on the second account. Passes when the second account's readings
are only its own, and the idle session's ticks after the swap were seen and dropped.

Usage (each account needs at least 20 minutes left on its access token; the stager checks):
  Windows:      uv run --with pywinpty --with pyte tests/e2e/idle-across-swap.py <labelA> <labelB>
  macOS/Linux:  uv run --with ptyprocess --with pyte tests/e2e/idle-across-swap.py <labelA> <labelB>
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
if len(sys.argv) != 3:
    sys.exit(__doc__)
label_a, label_b = sys.argv[1:]
if sorted([label_a, label_b]) != [label_a, label_b]:
    # The fake API gives the first label in sort order 40%, the other 5%.
    sys.exit("name the accounts in sort order, so the first one is the one in use first")
repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
t0 = time.time()
# Past the two minutes after a swap in which readings with the old account's reset times are
# dropped anyway (`STALE_SAMPLE_GUARD_MS`), plus a few status line refreshes (30 s).
IDLE_S = 180


def log(*parts):
    print(f"[{time.time() - t0:6.1f}s]", *parts, flush=True)


# ---- scratch store and profile ----------------------------------------------------------------

children = []
scratch = tempfile.mkdtemp(prefix="cc-rotate-e2e-idle-")


def cleanup():
    for child in children:
        try:
            child.terminate(True) if isinstance(child, PtyProcess) else child.kill()
        except Exception:
            pass
    deadline = time.time() + 15
    for child in children:
        alive = child.isalive if isinstance(child, PtyProcess) else (lambda c=child: c.poll() is None)
        while time.time() < deadline and alive():
            time.sleep(0.2)
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
    ["node", os.path.join(repo, "tests", "e2e", "fake-api.mjs"), token_map, "-"],
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


def count(match):
    with lock:
        return sum(1 for r in requests if match(r))


def wait_until(check, seconds):
    deadline = time.time() + seconds
    while time.time() < deadline:
        if check():
            return True
        time.sleep(0.5)
    return False


def query(sql, *params):
    db = sqlite3.connect(f"file:{os.path.join(data, 'store.db')}?mode=ro", uri=True)
    try:
        return db.execute(sql, params).fetchall()
    finally:
        db.close()


def readings(label, since_ms=0):
    """(ts, session, five_hour pct) of the account's status line readings taken from `since_ms`.
    Readings the user's other profiles hold of the account (section 9.1) are left out."""
    return query(
        "SELECT s.ts, s.session_id, w.pct FROM samples s JOIN accounts a ON a.id = s.account_id "
        "JOIN sample_windows w ON w.sample_id = s.id WHERE a.label = ? AND w.name = 'five_hour' "
        "AND s.profile = 'rotate' AND s.ts >= ? ORDER BY s.ts",
        label,
        since_ms,
    )


def diagnose():
    log("readings:", query(
        "SELECT a.label, s.ts, s.profile, s.session_id, w.name, w.pct FROM samples s "
        "JOIN accounts a ON a.id = s.account_id JOIN sample_windows w ON w.sample_id = s.id ORDER BY s.ts"
    ))
    try:
        log("session readings:", query("SELECT session_id, process, since_ms, seen_ms FROM session_readings"))
    except sqlite3.OperationalError as e:
        log("session readings:", e)
    log_file = os.path.join(data, "logs", "cc-rotate.log")
    if os.path.exists(log_file):
        log("cc-rotate.log:", open(log_file, encoding="utf-8", errors="replace").read()[-3000:])


# ---- two interactive Claude Code sessions ------------------------------------------------------

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


class Session:
    def __init__(self, name):
        self.name = name
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.Stream(self.screen)
        self.pty = PtyProcess.spawn([shutil.which("claude"), "--model", "haiku", "say pong"], cwd=work, env=env, dimensions=(rows, cols))
        children.append(self.pty)
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        while self.pty.isalive():
            try:
                self.stream.feed(self.pty.read(4096))
            except EOFError:
                break

    def show(self):
        print(f"--- {self.name} ---")
        print("\n".join(line.rstrip() for line in self.screen.display).strip("\n"), flush=True)


failures = []
idle, busy = Session("idle"), Session("busy")
streamed = lambda who: (lambda r: r.get("who") == who and r.get("status") == 200 and r.get("stream"))
if not wait_until(lambda: count(streamed(label_a)) >= 2, 120):
    sys.exit(f"FAIL: the sessions did not both answer on {label_a}")
log(f"both sessions answered on {label_a}")
# Both sessions show the same numbers, so the store keeps one reading of them; each session's
# status line notes its own (`session_readings`).
reporting = lambda: {s for (s,) in query("SELECT session_id FROM session_readings")}
if not readings(label_a) or not wait_until(lambda: len(reporting()) >= 2, 90):
    diagnose()
    sys.exit(f"FAIL: the status line did not record both sessions' readings of {label_a}")
log(f"{label_a} read by {len(reporting())} sessions")

log(ctl("swap", "--to", label_b))
swapped_ms = int(time.time() * 1000)
log(f"both sessions idle for {IDLE_S}s")
time.sleep(IDLE_S)
if count(lambda r: r.get("path") == "/v1/messages" and r["t"] >= swapped_ms):
    failures.append("a session sent a request while it should have idled")

busy.pty.write("say pong again")
time.sleep(1)
busy.pty.write("\r")
if not wait_until(lambda: count(streamed(label_b)) >= 1, 120):
    failures.append(f"the busy session did not send its message on {label_b}")
else:
    log(f"the busy session answered on {label_b}")
# A few more status line refreshes of both sessions.
time.sleep(75)

own = readings(label_b, swapped_ms)
log(f"{label_b}'s readings: {[(round((ts - swapped_ms) / 1000), str(s)[:8], pct) for ts, s, pct in own]}")
if not own:
    failures.append(f"no reading of {label_b} was recorded, not even the busy session's own")
if any(pct != 5 for _, _, pct in own):
    failures.append(f"{label_b} got another account's numbers: {sorted({pct for _, _, pct in own})}")
# The idle session's status line kept running after the swap (it shows the account in use, which
# cc-rotate prints first), while its numbers stayed the ones from before it; none were recorded.
idle_screen = "\n".join(idle.screen.display)
unchanged = query("SELECT session_id FROM session_readings WHERE since_ms < ?", swapped_ms)
if f"[{label_b}]" not in idle_screen:
    failures.append(f"the idle session's status line did not show {label_b} after the swap")
elif not unchanged:
    failures.append("no session still reported its numbers from before the swap (F24 not reproduced)")
else:
    log(f"the idle session still showed {label_a}'s numbers {IDLE_S}s and more after the swap; none recorded")
# Each session ran in one Claude Code process the whole time, and every tick told it the same.
processes = query("SELECT session_id, count(*) FROM session_processes GROUP BY session_id")
if len(processes) != 2 or any(n != 1 for _, n in processes):
    failures.append(f"the sessions' Claude Code processes were not told apart steadily: {processes}")

if failures:
    diagnose()
    idle.show()
    busy.show()
    for failure in failures:
        log("FAIL:", failure)
    sys.exit(1)
log("PASS")
