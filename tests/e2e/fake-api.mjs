// Fake Messages API for tests/e2e/wait-at-limit.py and idle-across-swap.py. The account whose
// access token hashes to the entry labelled <limited> in <map file> (or every account, for `*`;
// none, for `-`) reaches its 5-hour limit with the session's first turn:
// Claude Code's own checks before it pass (at 50%), and from that turn on it gets the usage-limit
// 429 Anthropic answers at a limit. Any other account gets a one-word answer. Model requests never reach Anthropic, so the test uses
// no quota. Prints its port, then one JSON line per request (never a token).
//
// With `-`, each account has its own 5-hour numbers, in the order of the labels sorted: the first
// 40% resetting at <reset>, the next 5% resetting half an hour later, and so on.
//
// usage: node fake-api.mjs <map file> <limited label> [<seconds until the limit resets>]
import http from "node:http";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

const [mapFile, limited, resetIn] = process.argv.slice(2);
const tokenMap = JSON.parse(readFileSync(mapFile, "utf8"));
const resetAt = Math.floor(Date.now() / 1000) + Number(resetIn ?? 3 * 3600);
const emit = (o) => process.stdout.write(JSON.stringify({ t: Date.now(), ...o }) + "\n");

let limitReached = false;

// Each account's 5-hour window has one reset time, as with Anthropic: the limited account's is
// `resetAt`, the other's a quarter of an hour later.
function answer(res, model, stream, utilization, reset) {
  const headers = {
    "anthropic-ratelimit-unified-status": "allowed",
    "anthropic-ratelimit-unified-5h-status": "allowed",
    "anthropic-ratelimit-unified-5h-utilization": String(utilization),
    "anthropic-ratelimit-unified-5h-reset": String(reset),
    "request-id": "req_e2e",
  };
  const message = {
    id: "msg_e2e",
    type: "message",
    role: "assistant",
    model,
    content: [],
    stop_reason: null,
    usage: { input_tokens: 5, output_tokens: 1 },
  };
  if (!stream) {
    res.writeHead(200, { "content-type": "application/json", ...headers });
    res.end(JSON.stringify({ ...message, content: [{ type: "text", text: "pong" }], stop_reason: "end_turn" }));
    return;
  }
  res.writeHead(200, { "content-type": "text/event-stream", ...headers });
  const send = (event, data) => res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  send("message_start", { type: "message_start", message });
  send("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } });
  send("content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "pong" } });
  send("content_block_stop", { type: "content_block_stop", index: 0 });
  send("message_delta", { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 1 } });
  send("message_stop", { type: "message_stop" });
  res.end();
}

const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    const token = String(req.headers.authorization ?? "").replace(/^Bearer /, "");
    const who = tokenMap[createHash("sha256").update(token).digest("hex")] ?? "?";
    const path = req.url.split("?")[0];
    let parsed = {};
    try {
      parsed = JSON.parse(body);
    } catch {}
    if (path !== "/v1/messages") {
      emit({ path, who, status: 404 });
      res.writeHead(404, { "content-type": "application/json" });
      res.end(JSON.stringify({ type: "error", error: { type: "not_found_error", message: "not here" } }));
      return;
    }
    const last = parsed.messages?.at(-1);
    const lastText = typeof last?.content === "string" ? last.content : (last?.content ?? []).map((b) => b.text ?? "").join(" ");
    const info = { path, who, stream: !!parsed.stream, max_tokens: parsed.max_tokens, last: lastText.slice(0, 200) };
    const isLimited = limited === "*" || who === limited;
    if (isLimited && parsed.stream) limitReached = true;
    if (isLimited && limitReached) {
      emit({ ...info, status: 429 });
      res.writeHead(429, {
        "content-type": "application/json",
        "anthropic-ratelimit-unified-status": "rejected",
        "anthropic-ratelimit-unified-reset": String(resetAt),
        "anthropic-ratelimit-unified-representative-claim": "five_hour",
        "anthropic-ratelimit-unified-5h-status": "rejected",
        "anthropic-ratelimit-unified-5h-utilization": "1.0",
        "anthropic-ratelimit-unified-5h-reset": String(resetAt),
      });
      res.end(
        JSON.stringify({
          type: "error",
          error: { type: "rate_limit_error", message: "This request would exceed your account's rate limit. Please try again later." },
        }),
      );
      return;
    }
    emit({ ...info, status: 200 });
    if (limited === "-") {
      const place = [...new Set(Object.values(tokenMap))].sort().indexOf(who);
      answer(res, parsed.model, !!parsed.stream, place === 0 ? 0.4 : 0.05, resetAt + 1800 * Math.max(place, 0));
    } else if (isLimited) answer(res, parsed.model, !!parsed.stream, 0.5, resetAt);
    else answer(res, parsed.model, !!parsed.stream, 0.1, resetAt + 900);
  });
});
server.listen(0, "127.0.0.1", () => emit({ port: server.address().port }));
