"""End-to-end check of relay mode (docs/specs/2026-11-10-relay-mode.md): the real Claude Code,
headless, sends its requests through this checkout's relay, which puts the active pool account's
login on them.

Everything lives in a scratch cc-rotate store and profile with three made-up accounts (a, the
home account, b and c; examples/e2e_stage.rs --made-up). The relay is a build with the
`relay-test-upstream` feature, pointed at the fake Messages API (fake-api.mjs) with
CC_ROTATE_RELAY_TEST_UPSTREAM, so no model request reaches Anthropic and no quota is spent.
No real login is read or used, the relay's service is never installed (CC_ROTATE_NO_SERVICE), and
the installed cc-rotate, its store and its profile are not touched.

Two fake APIs run: one where no account is at a limit, and one where b reaches its 5-hour limit
with its first turn (the fake's limit cannot be lifted once reached).

Scenarios:
- swap_retargets: with b active, a turn is served on b; after `swap --to c` the next one on c;
  the profile's login file never changes.
- resend_at_limit: b is at its limit; the relay sends the turn's refused request again on c and
  the turn completes; no StopFailure, no waiting session.
- relay_restart: the relay is killed between turns; the next turn, started through `cc-rotate`,
  succeeds on the relay the launcher starts.
- relay_down: the relay is killed and its program made unrunnable; a turn fails with a
  connection error ("Unable to connect to API", or `api_error: connection_failed`), and `doctor --json` reports the App account line as failing.
- version_check: Claude Code with the relay's two variables uses the stored login, its model
  requests come through the relay, and its debug file has no `custom_base_url` line. Two probe
  runs with nonessential traffic on (with and without `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`)
  read Claude Code's `[servedCatalog]` debug line, which says `not_first_party` when it does not
  take the base URL for Anthropic's; when it logs nothing either way, the script says the check
  is unverified.

Usage:  python3 tests/e2e/relay.py [--claude <path to claude>]
"""

import argparse
import hashlib
import json
import os
import shutil
import signal
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata

parser = argparse.ArgumentParser()
parser.add_argument("--claude", default=shutil.which("claude"))
opts = parser.parse_args()
if not opts.claude:
    sys.exit("claude not found: pass --claude <path>")
claude = os.path.realpath(opts.claude)

repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
exe = ".exe" if os.name == "nt" else ""
t0 = time.time()


def log(*parts):
    print(f"[{time.time() - t0:6.1f}s]", *parts, flush=True)


children = []
results = []


def relay_pids():
    """Every `relay run` started from this test's copy of cc-rotate-ctl, whoever started it."""
    out = subprocess.run(["pgrep", "-f", f"{scratch}/bin/cc-rotate-ctl relay run"], capture_output=True, text=True)
    return [int(p) for p in out.stdout.split()]


def kill_relays():
    for pid in relay_pids():
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    deadline = time.time() + 10
    while relay_pids() and time.time() < deadline:
        time.sleep(0.1)


def keychain_service():
    return "Claude Code-credentials-" + hashlib.sha256(unicodedata.normalize("NFC", profile).encode()).hexdigest()[:8]


def cleanup():
    for c in children:
        try:
            c.kill()
            c.wait(timeout=5)
        except Exception:
            pass
    kill_relays()
    if sys.platform == "darwin":
        # The test uses the file login; a Keychain item Claude Code made for the scratch profile
        # would outlive it.
        gone = subprocess.run(["/usr/bin/security", "delete-generic-password", "-s", keychain_service()], capture_output=True)
        if gone.returncode == 0:
            log("deleted a Keychain item Claude Code made for the scratch profile")
    left = subprocess.run(["pgrep", "-f", scratch], capture_output=True, text=True).stdout.split()
    if left:
        log("WARNING: processes still running from the scratch folder:", left)
        for pid in left:
            try:
                os.kill(int(pid), signal.SIGKILL)
            except Exception:
                pass
    shutil.rmtree(scratch, ignore_errors=True)


def tail(path, n=1200):
    try:
        return open(path, encoding="utf-8", errors="replace").read()[-n:]
    except OSError:
        return ""


class Failed(Exception):
    pass


def fail(why):
    raise Failed(why)


# ---- build and stage ---------------------------------------------------------------------------

subprocess.run(["cargo", "build", "-q", "-p", "ccr-cli", "--features", "relay-test-upstream"], cwd=repo, check=True)
subprocess.run(["cargo", "build", "-q", "-p", "ccr-core", "--example", "e2e_stage"], cwd=repo, check=True)
target = os.environ.get("CARGO_TARGET_DIR") or os.path.join(repo, "target")
stager = os.path.join(target, "debug", "examples", f"e2e_stage{exe}")
scratch = os.path.realpath(tempfile.mkdtemp(prefix="ccr-relay-e2e-"))

# The launcher and the relay run from a copy, which relay_down can make unrunnable without touching
# the build; cc-rotate finds cc-rotate-ctl next to itself. Commands run from a second copy.
bin_dir, ctl_dir = os.path.join(scratch, "bin"), os.path.join(scratch, "ctl")
for d in (bin_dir, ctl_dir):
    os.makedirs(d)
for name in ("cc-rotate", "cc-rotate-ctl"):
    shutil.copy2(os.path.join(target, "debug", name + exe), bin_dir)
shutil.copy2(os.path.join(target, "debug", "cc-rotate-ctl" + exe), ctl_dir)
launcher = os.path.join(bin_dir, "cc-rotate" + exe)
relay_exe = os.path.join(bin_dir, "cc-rotate-ctl" + exe)
ctl_exe = os.path.join(ctl_dir, "cc-rotate-ctl" + exe)

data, profile, template, work = (os.path.join(scratch, d) for d in ("data", "profile", "template", "work"))
token_map = os.path.join(scratch, "tokens.json")
stop_marker = os.path.join(scratch, "stop-failure-ran")
for d in (template, work):
    os.makedirs(d)
with open(os.path.join(template, "settings.json"), "w") as f:
    f.write("{}")

base_env = {k: v for k, v in os.environ.items() if not k.startswith(("CC_ROTATE_", "CLAUDE_", "ANTHROPIC_", "_CLAUDE_"))}
ctl_env = {**base_env, "CC_ROTATE_DATA_DIR": data, "CC_ROTATE_PROFILE": profile, "CC_ROTATE_NO_SERVICE": "1"}
staged = subprocess.run([stager, "--made-up", data, profile, token_map, "a", "b", "c"], env=ctl_env, capture_output=True, text=True)
if staged.returncode != 0:
    shutil.rmtree(scratch, ignore_errors=True)
    sys.exit(f"staging the made-up accounts failed: {staged.stderr.strip()}")
log(staged.stdout.strip())


def ctl(*args, ok=True):
    out = subprocess.run([ctl_exe, *args], env=ctl_env, capture_output=True, text=True, timeout=120)
    if ok and out.returncode != 0:
        fail(f"cc-rotate-ctl {' '.join(args)} failed: {out.stderr.strip()}")
    return out.stdout.strip()


def query(sql, *params):
    db = sqlite3.connect(f"file:{os.path.join(data, 'store.db')}?mode=ro", uri=True)
    try:
        return db.execute(sql, params).fetchall()
    finally:
        db.close()


def account_ids():
    return {label: i for i, label in query("select id, label from accounts")}


def active_label():
    row = query("select label from accounts where state = 'active'")
    return row[0][0] if row else None


creds_file = os.path.join(profile, ".credentials.json")


def creds_digest():
    with open(creds_file, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


# ---- the fake APIs -----------------------------------------------------------------------------

served = []  # (fake, entry, time)
served_lock = threading.Lock()


def start_fake(name, limited):
    api = subprocess.Popen(["node", os.path.join(repo, "tests", "e2e", "fake-api.mjs"), token_map, limited], stdout=subprocess.PIPE, text=True)
    children.append(api)
    port = json.loads(api.stdout.readline())["port"]

    def read():
        for line in api.stdout:
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            with served_lock:
                served.append((name, entry, time.time()))
            log(f"  {name} api: {entry.get('status')} {entry.get('path')} (prefix {entry.get('prefix')!r}) for account {entry.get('who')}")

    threading.Thread(target=read, daemon=True).start()
    return port


def messages_since(mark):
    with served_lock:
        return [(name, e) for name, e, _ in served[mark:] if e.get("path") == "/v1/messages"]


def mark():
    with served_lock:
        return len(served)


def answers(port):
    try:
        socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
        return True
    except OSError:
        return False


def upstream_env(port):
    return {**ctl_env, "CC_ROTATE_RELAY_TEST_UPSTREAM": f"http://127.0.0.1:{port}"}


def start_relay(port):
    relay = subprocess.Popen([relay_exe, "relay", "run"], env=upstream_env(port), cwd=data, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    children.append(relay)
    deadline = time.time() + 20
    while not answers(relay_port):
        if relay.poll() is not None:
            fail(f"the relay exited with {relay.returncode}: {tail(os.path.join(data, 'logs', 'relay.log'))}")
        if time.time() > deadline:
            fail("the relay never answered")
        time.sleep(0.05)
    log(f"relay serving on 127.0.0.1:{relay_port}, upstream fake on port {port}")
    return relay


def stop_relay():
    kill_relays()
    deadline = time.time() + 10
    while answers(relay_port):
        if time.time() > deadline:
            fail("the relay still answers after it was killed")
        time.sleep(0.05)


def session_env(upstream_port, extra=None):
    env = {
        **upstream_env(upstream_port),
        "DISABLE_AUTOUPDATER": "1",
        "DISABLE_TELEMETRY": "1",
        "DISABLE_ERROR_REPORTING": "1",
        # The made-up logins would be refused by the account lookups Claude Code makes straight
        # to Anthropic, and the turn would stop as signed out. Off here, as in
        # desktop-wait-at-limit.py; it turns off artifacts, which nothing here tests.
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    }
    env.update(extra or {})
    return env


def turn(name, env, args=None, timeout=240):
    """One headless turn; returns (result JSON or None, stdout, stderr)."""
    cmd = args or [launcher, "-p", "say pong", "--model", "haiku", "--output-format", "json"]
    try:
        out = subprocess.run(cmd, cwd=work, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired as e:
        fail(f"{name}: the turn did not finish within {timeout} s (stderr: {(e.stderr or '')[-400:]!r})")
    result = None
    for line in out.stdout.splitlines():
        try:
            parsed = json.loads(line)
        except ValueError:
            continue
        if isinstance(parsed, dict) and parsed.get("type") == "result":
            result = parsed
    log(f"  {name}: exit {out.returncode}, is_error={result and result.get('is_error')}, result={str(result and result.get('result'))[:100]!r}")
    return result, out.stdout, out.stderr


def succeeded(name, result, stdout, stderr):
    if result is None or result.get("is_error"):
        fail(f"{name}: the turn failed: {stdout[-600:]!r} {stderr[-600:]!r}")


def served_on(name, lines, label):
    if not lines:
        fail(f"{name}: no model request reached the fake API")
    wrong = [(f, e.get("who"), e.get("status")) for f, e in lines if e.get("who") != label or e.get("status") != 200]
    if wrong:
        fail(f"{name}: every model request should be served on {label}, the fake API saw {wrong}")
    # `lines` are requests whose path is exactly `/v1/messages`, so each arrived with the relay's
    # prefix taken off; there is nothing more to check about the prefix here.


def prepare():
    global open_port, limit_port, relay_port, relay_base
    open_port = start_fake("open", "-")
    limit_port = start_fake("b-limited", "b")

    ctl("setup", "--from", template)
    ctl("config", "claude_path", claude)
    log(ctl("swap", "--to", "a"))

    settings_file = os.path.join(profile, "settings.json")
    settings = json.load(open(settings_file, encoding="utf-8"))
    # Tells whether Claude Code ran its StopFailure hooks, cc-rotate's among them.
    settings.setdefault("hooks", {}).setdefault("StopFailure", []).append(
        {"hooks": [{"type": "command", "command": f"{sys.executable} -c \"open({stop_marker!r}, 'a').write('ran\\n')\""}]}
    )
    json.dump(settings, open(settings_file, "w", encoding="utf-8"))
    config_file = os.path.join(profile, ".claude.json")
    config = json.load(open(config_file, encoding="utf-8")) if os.path.exists(config_file) else {}
    config.update(
        hasCompletedOnboarding=True,
        projects={os.path.realpath(work).replace("\\", "/"): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}},
    )
    json.dump(config, open(config_file, "w", encoding="utf-8"))

    log(ctl("app-account", "a", "--no-service"))
    relay_cfg = json.load(open(os.path.join(data, "relay.json"), encoding="utf-8"))
    relay_port = relay_cfg["port"]
    relay_base = f"http://127.0.0.1:{relay_port}/r/{relay_cfg['secret']}"
    # The home account takes no turns: a swap away from a refused account goes to c.
    log(ctl("swap", "--to", "b"))
    log(ctl("pause", "a"))


# ---- the scenarios -----------------------------------------------------------------------------


def swap_retargets():
    log(ctl("swap", "--to", "b"))
    before = creds_digest()
    start_relay(open_port)
    m = mark()
    result = turn("turn 1", session_env(open_port))
    succeeded("turn 1", result[0], *result[1:])
    served_on("turn 1", messages_since(m), "b")
    if creds_digest() != before:
        fail("the profile's login file changed during turn 1")
    log(ctl("swap", "--to", "c"))
    if creds_digest() != before:
        fail("the swap to c changed the profile's login file")
    m = mark()
    result = turn("turn 2", session_env(open_port))
    succeeded("turn 2", result[0], *result[1:])
    served_on("turn 2", messages_since(m), "c")
    if creds_digest() != before:
        fail("the profile's login file changed during turn 2")
    ids = account_ids()
    relay_samples = {r[0] for r in query("select account_id from samples where profile = 'relay'")}
    if not {ids["b"], ids["c"]} <= relay_samples:
        fail(f"the relay should record readings of b and c, it has readings of {sorted(relay_samples)}")
    log("turns follow the active account; the profile keeps the home login; the relay records readings")


def resend_at_limit():
    stop_relay()
    log(ctl("swap", "--to", "b"))
    start_relay(limit_port)
    since = int(time.time() * 1000)
    m = mark()
    result = turn("turn at b's limit", session_env(limit_port))
    succeeded("turn at b's limit", result[0], *result[1:])
    model = [(e.get("who"), e.get("status")) for f, e in messages_since(m) if e.get("stream")]
    if ("b", 429) not in model or ("c", 200) not in model or model.index(("b", 429)) > model.index(("c", 200)):
        fail(f"the turn's request should be refused on b, then served on c; the fake API saw {model}")
    ids = account_ids()
    resends = query("select account_from, account_to from events where kind = 'relay_resend' and ts >= ?", since)
    if (ids["b"], ids["c"]) not in resends:
        fail(f"the store should have a relay_resend from b to c, it has {resends}")
    time.sleep(3)  # A StopFailure hook would have run by now.
    if os.path.exists(stop_marker):
        fail("Claude Code ran its StopFailure hooks")
    waiting = query("select session_id, state from waiting_sessions")
    stopped = query("select reason from events where kind = 'session' and ts >= ?", since)
    if waiting or stopped:
        fail(f"no session should wait: waiting sessions {waiting}, session events {stopped}")
    if active_label() != "c":
        fail(f"the relay should have swapped to c, the active account is {active_label()}")
    log("the relay resent the refused request on c; the turn completed without a StopFailure")


def relay_restart():
    stop_relay()
    log("relay killed; the next turn starts through cc-rotate")
    m = mark()
    result = turn("turn after the kill", session_env(limit_port))
    succeeded("turn after the kill", result[0], *result[1:])
    served_on("turn after the kill", messages_since(m), "c")
    pids = relay_pids()
    if not pids or not answers(relay_port):
        fail("the launcher should have started a relay that still serves")
    log(f"the launcher started the relay again (pid {pids})")


def relay_down():
    stop_relay()
    os.chmod(relay_exe, stat.S_IRUSR | stat.S_IWUSR)
    try:
        m = mark()
        # Claude Code retries a connection error ten times by default, for minutes.
        result, stdout, stderr = turn("turn without a relay", session_env(limit_port, {"CLAUDE_CODE_MAX_RETRIES": "1"}))
        if answers(relay_port) or relay_pids():
            fail("a relay started although its program cannot run")
        if result is not None and not result.get("is_error"):
            fail(f"the turn should fail without a relay, it answered {result.get('result')!r}")
        # Older Claude Code says "Unable to connect to API"; 2.1.3xx says "Connection refused"
        # and marks the result `api_error: connection_failed`.
        said = stdout + stderr
        if not ("Unable to connect to API" in said or (result or {}).get("api_error") == "connection_failed"):
            fail(f"the turn should report a connection error: {stdout[-600:]!r} {stderr[-600:]!r}")
        if messages_since(m):
            fail(f"no request should reach the fake API, it saw {messages_since(m)}")
        doctor = json.loads(ctl("doctor", "--json", ok=False))
        checks = doctor.get("checks", doctor) if isinstance(doctor, dict) else doctor
        line = next((c for c in checks if c.get("name") == "App account"), None)
        if line is None or line.get("status") != "fail" or "doesn't answer" not in line.get("detail", ""):
            fail(f"doctor's App account line should fail because the relay doesn't answer: {line}")
        log(f"doctor: App account {line['status']}: {line['detail'][:160]}")
    finally:
        os.chmod(relay_exe, stat.S_IRWXU)


def version_check():
    start_relay(limit_port)
    first_party = {"ANTHROPIC_BASE_URL": relay_base, "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL": "1", "CLAUDE_CONFIG_DIR": profile}

    def run(name, variables, debug):
        env = session_env(limit_port, variables)
        return turn(name, env, [claude, "-p", "say pong", "--model", "haiku", "--output-format", "json", "--debug-file", debug])

    debug = os.path.join(scratch, "debug.txt")
    m = mark()
    result, stdout, stderr = run("claude with the relay's variables", first_party, debug)
    if "Not logged in" in stdout + stderr:
        fail("Claude Code says it is not logged in with the relay's variables")
    succeeded("claude with the relay's variables", result, stdout, stderr)
    # The profile has a's login; c on the fake API means the relay put the active account's on.
    served_on("claude with the relay's variables", messages_since(m), "c")
    text = tail(debug, 10**8)
    if not text.strip():
        fail(f"Claude Code wrote no debug file at {debug}, so custom_base_url can't be checked")
    lines = [l for l in text.splitlines() if "custom_base_url" in l]
    if lines:
        fail(f"Claude Code's debug file mentions custom_base_url: {lines[:3]}")

    # Does Claude Code take the relay for Anthropic? With nonessential traffic off (as above) it
    # logs nothing about it. With it on, it logs whether it serves the model catalog, and why
    # not: `not_first_party` when it takes the base URL for someone else's. Those runs then stop
    # signed out, after Claude Code's account lookups to Anthropic refuse the made-up login and
    # its renewal fails; only their debug files are read. The login file and any Keychain item
    # they leave are put back or removed after each.
    saved = open(creds_file, "rb").read()
    verdicts = {}
    without = {k: v for k, v in first_party.items() if not k.startswith("_")}
    for name, variables in (("with", first_party), ("without", without)):
        probe = os.path.join(scratch, f"debug-probe-{name}.txt")
        m = mark()
        env = session_env(limit_port, variables)
        del env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"]
        turn(f"probe {name} the first-party variable", env, [claude, "-p", "say pong", "--model", "haiku", "--output-format", "json", "--debug-file", probe])
        with open(creds_file, "wb") as f:
            f.write(saved)
        if sys.platform == "darwin":
            subprocess.run(["/usr/bin/security", "delete-generic-password", "-s", keychain_service()], capture_output=True)
        if [e for _, e in messages_since(m) if e.get("status") == 200]:
            log(f"  (the probe {name} the variable reached the model)")
        text = tail(probe, 10**8).splitlines()
        if name == "with" and any("custom_base_url" in l for l in text):
            fail(f"with the relay's variables, Claude Code's debug file mentions custom_base_url: {[l for l in text if 'custom_base_url' in l][:3]}")
        catalog = [l.split("[servedCatalog]", 1)[1].strip() for l in text if "[servedCatalog]" in l]
        verdicts[name] = catalog
        log(f"  servedCatalog {name} the variable: {catalog}")
    if not verdicts["with"] and not verdicts["without"]:
        log("first-party check UNVERIFIED: Claude Code logs nothing about it either way (Task 13's model-picker step decides)")
    elif any("not_first_party" in v or "custom_base_url" in v for v in verdicts["with"]):
        fail(f"with the relay's variables, Claude Code takes the relay for another provider: {verdicts['with']}")
    elif not any("not_first_party" in v or "custom_base_url" in v for v in verdicts["without"]):
        log(f"first-party check UNVERIFIED: Claude Code logs the same without the variable ({verdicts['without']})")
    else:
        log("first-party check verified: Claude Code takes the relay for Anthropic only with the variable")


def report_failure(e):
    log("FAIL:", e)
    log("relay log tail:", tail(os.path.join(data, "logs", "relay.log")))
    log("cc-rotate log tail:", tail(os.path.join(data, "logs", "cc-rotate.log")))


scenarios = [swap_retargets, resend_at_limit, relay_restart, relay_down, version_check]
failed = False
try:
    try:
        prepare()
    except Failed as e:
        report_failure(e)
        scenarios = []
        failed = True
    for scenario in scenarios:
        log(f"== {scenario.__name__}")
        try:
            scenario()
            results.append((scenario.__name__, "PASS", ""))
        except Failed as e:
            failed = True
            results.append((scenario.__name__, "FAIL", str(e)))
            report_failure(e)
            break
except BaseException:
    cleanup()
    raise
cleanup()
for name, outcome, why in results:
    print(f"{outcome}  {name}{'  ' + why if why else ''}")
for scenario in scenarios[len(results):]:
    print(f"SKIP  {scenario.__name__}")
print("FAIL" if failed or len(results) < len(scenarios) else "PASS")
sys.exit(1 if failed else 0)
