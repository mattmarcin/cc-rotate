"""End-to-end check of desktop app sessions (spec section 24): a Code session the Claude desktop
app starts through cc-rotate's launcher runs on the pool, and continues on the next account after
stopping at a usage limit.

Runs the real Claude Code the way the desktop app runs it in launcher mode: headless, stream-json
in and out, `CLAUDE_CODE_ENTRYPOINT=claude-desktop` with the app's host marks and no login, started
through this checkout's `cc-rotate-ctl desktop-launch`. Everything lives in a scratch cc-rotate
store and profile with two made-up accounts (examples/e2e_stage.rs --made-up), and every request
goes to the fake API (fake-api.mjs), which puts the first account at its 5-hour limit. No real
login is read or used, and no quota is spent. The installed cc-rotate, its store and its profile
are not touched.

Passes when:
- the session starts on the cc-rotate profile, with the six variables the launcher removes gone;
- its first turn stops at the first account's limit;
- the StopFailure hook swaps to the second account and wakes the session, whose turn runs on it;
- the Stop hook marks the stop resumed, which a desktop session has no status line to do.

With --keychain (macOS), the profile's login is kept in its Keychain item, as Claude Code started
by the app keeps it, so the wake must wait out Claude Code's 30 s read cache; the item is deleted
at the end.

Usage:  python3 tests/e2e/desktop-wait-at-limit.py [--keychain] [--claude <path to claude>]
"""

import argparse
import hashlib
import json
import os
import queue
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata

REMOVED = [
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    "CLAUDE_CODE_HOST_AUTH_ENV_VAR",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_DISABLE_FAST_MODE",
]

parser = argparse.ArgumentParser()
parser.add_argument("--keychain", action="store_true")
parser.add_argument("--claude", default=shutil.which("claude"))
opts = parser.parse_args()
if not opts.claude:
    sys.exit("claude not found: pass --claude <path>")
if opts.keychain and sys.platform != "darwin":
    sys.exit("--keychain is for macOS")

repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
exe = ".exe" if os.name == "nt" else ""
t0 = time.time()


def log(*parts):
    print(f"[{time.time() - t0:6.1f}s]", *parts, flush=True)


children = []
keychain_item = None
scratch = tempfile.mkdtemp(prefix="ccr-desktop-e2e-")


def cleanup():
    for c in children:
        try:
            c.kill()
        except Exception:
            pass
    if keychain_item:
        subprocess.run(["/usr/bin/security", "delete-generic-password", "-a", keychain_item[0], "-s", keychain_item[1]], capture_output=True)
    shutil.rmtree(scratch, ignore_errors=True)


def fail(why):
    log("FAIL:", why)
    try:
        tail = open(os.path.join(scratch, "claude.err"), encoding="utf-8", errors="replace").read()[-800:]
        if tail.strip():
            log("claude stderr tail:", tail)
        logfile = os.path.join(scratch, "data", "logs", "cc-rotate.log")
        if os.path.exists(logfile):
            log("cc-rotate log tail:", open(logfile, encoding="utf-8").read()[-800:])
    finally:
        cleanup()
    sys.exit(1)


subprocess.run(["cargo", "build", "-q", "-p", "ccr-cli"], cwd=repo, check=True)
subprocess.run(["cargo", "build", "-q", "-p", "ccr-core", "--example", "e2e_stage"], cwd=repo, check=True)
target = os.environ.get("CARGO_TARGET_DIR") or os.path.join(repo, "target")
ctl_exe = os.path.join(target, "debug", f"cc-rotate-ctl{exe}")
stager = os.path.join(target, "debug", "examples", f"e2e_stage{exe}")

data, profile, template, work, app_profile = (os.path.join(scratch, d) for d in ("data", "profile", "template", "work", "app-profile"))
token_map = os.path.join(scratch, "tokens.json")
for d in (template, work, app_profile):
    os.makedirs(d)
with open(os.path.join(template, "settings.json"), "w") as f:
    f.write("{}")

base_env = {k: v for k, v in os.environ.items() if not k.startswith(("CC_ROTATE_", "CLAUDE_", "ANTHROPIC_"))}
ctl_env = {**base_env, "CC_ROTATE_DATA_DIR": data, "CC_ROTATE_PROFILE": profile}
log(subprocess.run([stager, "--made-up", data, profile, token_map, "a", "b"], env=ctl_env, capture_output=True, text=True, check=True).stdout.strip())


def ctl(*args):
    out = subprocess.run([ctl_exe, *args], env=ctl_env, capture_output=True, text=True)
    if out.returncode != 0:
        fail(f"cc-rotate-ctl {' '.join(args)} failed: {out.stderr.strip()}")
    return out.stdout.strip()


ctl("setup", "--from", template)
if opts.keychain:
    # Claude Code started by the app moves the login into this item (F31); cc-rotate then writes
    # every swap there.
    service = "Claude Code-credentials-" + hashlib.sha256(unicodedata.normalize("NFC", profile).encode()).hexdigest()[:8]
    keychain_item = (os.environ.get("USER", ""), service)
    subprocess.run(["/usr/bin/security", "add-generic-password", "-U", "-a", keychain_item[0], "-s", service, "-w", "{}"], check=True, capture_output=True)
    log("Keychain item for the scratch profile:", service)
log(ctl("swap", "--to", "a"))

# Desktop sessions run in the profile; this records what the session's environment holds (names,
# and CLAUDE_CONFIG_DIR's value) with a hook of its own.
env_dump = os.path.join(scratch, "session-env.json")
settings_file = os.path.join(profile, "settings.json")
settings = json.load(open(settings_file, encoding="utf-8"))
dump = f"{sys.executable} -c \"import json,os; json.dump({{'names': sorted(os.environ), 'config_dir': os.environ.get('CLAUDE_CONFIG_DIR')}}, open({env_dump!r}, 'w'))\""
settings.setdefault("hooks", {}).setdefault("SessionStart", []).append({"hooks": [{"type": "command", "command": dump}]})
# The made-up logins would be refused by the account lookups Claude Code makes to Anthropic, which
# do not follow ANTHROPIC_BASE_URL. The launcher removes the app's switch for them; set in the
# profile's own settings, as a user may (spec section 24.1), it keeps the test off Anthropic.
settings.setdefault("env", {})["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"] = "1"
json.dump(settings, open(settings_file, "w", encoding="utf-8"))
config_file = os.path.join(profile, ".claude.json")
config = json.load(open(config_file, encoding="utf-8")) if os.path.exists(config_file) else {}
config.update(
    hasCompletedOnboarding=True,
    projects={os.path.realpath(work).replace("\\", "/"): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}},
)
json.dump(config, open(config_file, "w", encoding="utf-8"))

# ---- the fake API ------------------------------------------------------------------------------

api = subprocess.Popen(["node", os.path.join(repo, "tests", "e2e", "fake-api.mjs"), token_map, "a"], stdout=subprocess.PIPE, text=True)
children.append(api)
port = json.loads(api.stdout.readline())["port"]
served = []


def read_api():
    for line in api.stdout:
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if entry.get("path") == "/v1/messages":
            served.append((entry.get("who"), entry.get("status"), time.time()))
            log(f"  api: {entry.get('status')} for account {entry.get('who')}")


threading.Thread(target=read_api, daemon=True).start()

# ---- the session, as the app starts it in launcher mode -------------------------------------

session_env = {
    **ctl_env,
    "CLAUDE_CODE_ENTRYPOINT": "claude-desktop",
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST": "1",
    "CLAUDE_CODE_HOST_AUTH_ENV_VAR": "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_DISABLE_FAST_MODE": "1",
    # The app's own profile, which the launcher replaces.
    "CLAUDE_CONFIG_DIR": app_profile,
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{port}",
    "DISABLE_AUTOUPDATER": "1",
    "DISABLE_TELEMETRY": "1",
    "DISABLE_ERROR_REPORTING": "1",
}
args = [ctl_exe, "desktop-launch", "--", opts.claude, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--model", "haiku", "--setting-sources=user,project,local"]
err = open(os.path.join(scratch, "claude.err"), "w")
session = subprocess.Popen(args, cwd=work, env=session_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True)
children.append(session)
events = queue.Queue()


def read_session():
    for line in session.stdout:
        try:
            events.put(json.loads(line))
        except ValueError:
            pass
    events.put(None)


threading.Thread(target=read_session, daemon=True).start()


def next_result(timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            ev = events.get(timeout=max(0.1, deadline - time.time()))
        except queue.Empty:
            return None
        if ev is None:
            return None
        if ev.get("type") == "result":
            log(f"  result: is_error={ev.get('is_error')} text={str(ev.get('result'))[:80]!r}")
            return ev
    return None


def waiting_state(session_id):
    db = sqlite3.connect(f"file:{os.path.join(data, 'store.db')}?mode=ro", uri=True)
    try:
        row = db.execute("select state from waiting_sessions where session_id = ? order by since_ms desc limit 1", (session_id,)).fetchone()
        return row[0] if row else None
    finally:
        db.close()


try:
    session.stdin.write(json.dumps({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "say pong"}]}}) + "\n")
    session.stdin.flush()
    first = next_result(120)
    if first is None:
        fail("no result for the first turn")
    session_id = first.get("session_id")

    seen = json.load(open(env_dump)) if os.path.exists(env_dump) else None
    if seen is None:
        fail("the session's SessionStart hook did not run")
    if os.path.realpath(seen["config_dir"] or "") != os.path.realpath(profile):
        fail(f"the session ran in {seen['config_dir']}, not the cc-rotate profile")
    # The profile's settings set the traffic switch again on purpose (above).
    left = [n for n in REMOVED if n in seen["names"] and n != "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"]
    if left:
        fail(f"the launcher left {left} in the session's environment")
    log("the session runs in the cc-rotate profile, without the app's host marks")
    if not served or served[0][:2] != ("a", 429):
        fail(f"the first turn should stop at a's limit, the API saw {served}")

    log("waiting for the StopFailure hook to swap to b and wake the session")
    stopped_at = served[0][2]
    second = next_result(150)
    if second is None:
        fail("the session was not woken")
    woken = [s for s in served if s[0] == "b"]
    if not woken or woken[0][1] != 200 or second.get("is_error"):
        fail(f"the woken turn should run on b, the API saw {served}")
    log(f"the woken turn ran on b, {woken[0][2] - stopped_at:.0f} s after the stop")
    if opts.keychain and woken[0][2] - stopped_at < 31:
        fail("the wake did not wait for Claude Code's Keychain read cache")

    deadline = time.time() + 15
    state = None
    while time.time() < deadline:
        state = waiting_state(session_id)
        if state == "resumed":
            break
        time.sleep(0.5)
    if state != "resumed":
        fail(f"the stop should be marked resumed by the Stop hook, it is {state!r}")
    log("the Stop hook marked the stop resumed")
finally:
    try:
        session.stdin.close()
    except Exception:
        pass

cleanup()
print("PASS")
