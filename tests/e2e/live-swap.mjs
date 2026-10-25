// Live end-to-end check against the real Claude Code and real accounts.
//
// Keeps ONE `claude` process alive (stream-json in and out) and sends a tiny prompt per turn.
// Between turns it swaps the rotation profile to another account with `cc-rotate-ctl swap --to`.
// Each turn's `rate_limit_event` carries that account's window reset times, which differ per
// account, so the test can tell which account served each turn without trusting cc-rotate.
//
// Usage (needs at least two accounts in the pool; uses a few tiny requests per account):
//   node tests/e2e/live-swap.mjs <labelA> <labelB> [--force-refresh] [--settle <seconds>]
//
// --force-refresh marks the first account's access token as expired just before swapping away,
// so swapping back makes Claude Code renew a parked login from the stored refresh token (E3).
// It edits `.credentials.json`, so it is not available on macOS, where the login lives in the
// Keychain. Never use it with a login that another machine or profile also holds: the renewal
// retires the other copies.
//
// --settle waits after each swap before the next turn. Claude Code caches the macOS Keychain for
// 30 s, so it defaults to 35 on macOS and 0 elsewhere.

import { spawn, execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

const argv = process.argv.slice(2);
const settleAt = argv.indexOf("--settle");
const settleSeconds = settleAt >= 0 ? Number(argv[settleAt + 1]) : process.platform === "darwin" ? 35 : 0;
const positional = argv.filter((a, i) => !a.startsWith("--") && !(settleAt >= 0 && i === settleAt + 1));
const [labelA, labelB] = positional;
const forceRefresh = argv.includes("--force-refresh");
if (!labelA || !labelB || !Number.isFinite(settleSeconds)) {
  console.error("usage: node tests/e2e/live-swap.mjs <labelA> <labelB> [--force-refresh] [--settle <seconds>]");
  process.exit(2);
}
if (forceRefresh && process.platform === "darwin") {
  console.error("--force-refresh edits .credentials.json and is not available on macOS");
  process.exit(2);
}
const settle = () => new Promise((resolve) => setTimeout(resolve, settleSeconds * 1000));

const ctl = (...args) => execFileSync("cc-rotate-ctl", args, { encoding: "utf8" }).trim();
const profile = ctl("config", "profile_dir") || join(homedir(), ".claude-rotate");
const credFile = join(profile, ".credentials.json");

function startClaude() {
  const child = spawn(
    "claude",
    ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--model", "haiku"],
    { env: { ...process.env, CLAUDE_CONFIG_DIR: profile }, stdio: ["pipe", "pipe", "inherit"], shell: true },
  );
  let buffer = "";
  const waiters = [];
  const events = [];
  child.stdout.on("data", (chunk) => {
    buffer += chunk;
    let nl;
    while ((nl = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, nl).trim();
      buffer = buffer.slice(nl + 1);
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      events.push(msg);
      if (msg.type === "result") waiters.shift()?.(msg);
    }
  });
  return {
    async turn(text) {
      const start = events.length;
      const done = new Promise((resolve) => waiters.push(resolve));
      child.stdin.write(JSON.stringify({ type: "user", message: { role: "user", content: text } }) + "\n");
      const result = await done;
      const limits = events
        .slice(start)
        .filter((e) => e.type === "rate_limit_event")
        .map((e) => e.rate_limit_info);
      return { result, limits };
    },
    close() {
      child.stdin.end();
    },
  };
}

/// The reset times of every window the serving account reported, e.g. "five_hour@1793...".
/// Accounts start their windows at different moments, so these identify the account. Not every
/// plan has every window (some have no weekly limit), so all reported windows are compared.
function fingerprint(limits) {
  const info = limits.findLast((l) => l?.unifiedWindows) ?? limits.at(-1);
  if (!info) return null;
  const windows = Object.entries(info.unifiedWindows ?? {})
    .filter(([, w]) => w && Number.isFinite(w.resetsAt))
    .map(([name, w]) => `${name}@${w.resetsAt}`)
    .sort();
  if (windows.length === 0 && Number.isFinite(info.resetsAt)) windows.push(`${info.rateLimitType ?? "limit"}@${info.resetsAt}`);
  return windows.length ? windows.join(" ") : null;
}

function expireActiveLogin() {
  const doc = JSON.parse(readFileSync(credFile, "utf8"));
  doc.claudeAiOauth.expiresAt = Date.now() - 60_000;
  writeFileSync(credFile, JSON.stringify(doc));
}

const results = [];
function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
}

ctl("swap", "--to", labelA);
const claude = startClaude();
try {
  const t1 = await claude.turn("Reply with the single word: one");
  const fpA = fingerprint(t1.limits);
  check("turn 1 succeeded on " + labelA, !t1.result.is_error, t1.result.subtype);
  check("turn 1 reported its limits", !!fpA, `${fpA}`);

  if (forceRefresh) expireActiveLogin();
  const swapOut = ctl("swap", "--to", labelB);
  check(`swapped to ${labelB}`, swapOut.includes(labelB), swapOut);
  await settle();

  const t2 = await claude.turn("Reply with the single word: two");
  const fpB = fingerprint(t2.limits);
  check("turn 2 succeeded in the same process", !t2.result.is_error, t2.result.subtype);
  check(
    "turn 2 was served by a different account (same process, no restart)",
    !!fpB && fpB !== fpA,
    `A ${fpA}, B ${fpB}`,
  );

  ctl("swap", "--to", labelA);
  await settle();
  const t3 = await claude.turn("Reply with the single word: three");
  const fpA2 = fingerprint(t3.limits);
  check("turn 3 succeeded after swapping back", !t3.result.is_error, t3.result.subtype);
  check("turn 3 was served by the first account again", !!fpA2 && fpA2 === fpA, `${fpA2}`);
} finally {
  claude.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
