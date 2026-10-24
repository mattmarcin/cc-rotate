// Live end-to-end check against the real Claude Code and real accounts.
//
// Keeps ONE `claude` process alive (stream-json in and out) and sends a tiny prompt per turn.
// Between turns it swaps the rotation profile to another account with `cc-rotate-ctl swap --to`.
// Each turn's `rate_limit_event` carries that account's window reset times, which differ per
// account, so the test can tell which account served each turn without trusting cc-rotate.
//
// Usage (needs at least two accounts in the pool; uses a few tiny requests per account):
//   node tests/e2e/live-swap.mjs <labelA> <labelB> [--force-refresh]
//
// --force-refresh marks the first account's access token as expired just before swapping away,
// so swapping back makes Claude Code renew a parked login from the stored refresh token (E3).

import { spawn, execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

const [labelA, labelB] = process.argv.slice(2).filter((a) => !a.startsWith("--"));
const forceRefresh = process.argv.includes("--force-refresh");
if (!labelA || !labelB) {
  console.error("usage: node tests/e2e/live-swap.mjs <labelA> <labelB> [--force-refresh]");
  process.exit(2);
}

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

function fingerprint(limits) {
  const info = limits.findLast((l) => l?.unifiedWindows) ?? limits.at(-1);
  if (!info) return null;
  const w = info.unifiedWindows ?? {};
  return {
    weekReset: w.seven_day?.resetsAt ?? null,
    fiveReset: w.five_hour?.resetsAt ?? null,
    weekUsed: w.seven_day?.utilization ?? null,
  };
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
  check("turn 1 reported its limits", !!fpA?.weekReset, JSON.stringify(fpA));

  if (forceRefresh) expireActiveLogin();
  const swapOut = ctl("swap", "--to", labelB);
  check(`swapped to ${labelB}`, swapOut.includes(labelB), swapOut);

  const t2 = await claude.turn("Reply with the single word: two");
  const fpB = fingerprint(t2.limits);
  check("turn 2 succeeded in the same process", !t2.result.is_error, t2.result.subtype);
  check(
    "turn 2 was served by a different account (same process, no restart)",
    !!fpB?.weekReset && fpB.weekReset !== fpA?.weekReset,
    `A week reset ${fpA?.weekReset}, B week reset ${fpB?.weekReset}`,
  );

  ctl("swap", "--to", labelA);
  const t3 = await claude.turn("Reply with the single word: three");
  const fpA2 = fingerprint(t3.limits);
  check("turn 3 succeeded after swapping back", !t3.result.is_error, t3.result.subtype);
  check("turn 3 was served by the first account again", fpA2?.weekReset === fpA?.weekReset, `week reset ${fpA2?.weekReset}`);
} finally {
  claude.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
