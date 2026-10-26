// What the activity list says about each event in the store's log.
import type { EventRow } from "./api";
import { h, type Child } from "./dom";

/** Events left out unless "Show all" is on: renewals, skipped swaps, version notes, warm-ups, reorders. */
const ROUTINE = new Set(["sync", "swap_skipped", "claude_version", "warm_up"]);
export const routine = (e: EventRow) => ROUTINE.has(e.kind) || (e.kind === "state" && e.reason === "order");

export const REASONS: Record<string, string> = {
  threshold_5h: "at the 5-hour limit",
  threshold_7d: "at the weekly limit",
  threshold_other: "at a usage limit",
  preferred_available: "now that it's ready again",
  stop_failure: "after Claude Code hit a limit",
  avoid_extra_usage: "so sessions wait instead of paying for extra usage",
  dead_login: "because the login stopped working",
  manual: "by hand",
  login: "after its first sign-in",
  completed: "after an interrupted swap",
};

/** A usage check's outcome, as a swap event lists it. A check that read the usage needs no note. */
const CHECKED: Record<string, string> = {
  read: "",
  skipped: "skipped",
  rate_limited: "asked to wait",
  login_rejected: "login rejected",
  failed: "check failed",
};

const nb = (text: string) => h("span", { class: "nb" }, text);

export function describe(e: EventRow, labelOf: (id: number | null) => string): { parts: Child[]; tone?: "warn" | "danger" } {
  const from = nb(labelOf(e.account_from));
  const to = nb(labelOf(e.account_to));
  const detail = (k: string) => (e.detail?.[k] as string | undefined) ?? "";
  switch (e.kind) {
    case "swap": {
      const pick = detail("pick");
      const why = pick ? h("small", { class: "why" }, `Picked: ${pick}`) : null;
      const arrived = e.detail?.arrived_over_limit
        ? h("small", { class: "why" }, `${labelOf(e.account_from)} was already at this limit when it was swapped in`)
        : null;
      const checkedList = (e.detail?.checked as { label: string; outcome: string }[] | undefined) ?? [];
      const checked = checkedList.length
        ? h("small", { class: "why" }, `Checked first: ${checkedList.map((c) => { const note = CHECKED[c.outcome] ?? c.outcome; return note ? `${c.label} (${note})` : c.label; }).join(", ")}`)
        : null;
      return e.account_from
        ? { parts: ["Swapped from ", from, " to ", to, ` ${REASONS[e.reason ?? ""] ?? ""}`.trimEnd(), arrived, checked, why] }
        : { parts: ["Started using ", to, why] };
    }
    case "swap_skipped":
      return { parts: [`No swap: ${detail("why")}`] };
    case "login":
      return { parts: e.reason === "added" ? ["Added ", to] : ["Signed in to ", to, " again"] };
    case "sync":
      return e.reason === "usage_check" || e.reason === "recovered"
        ? { parts: ["Saved ", from, "'s renewed login after a usage check"] }
        : { parts: ["Saved ", from, "'s renewed login"] };
    case "dead_login":
      return e.reason === "sign_in_ended"
        ? { parts: [from, "'s sign-in ended"], tone: "danger" }
        : { parts: [from, "'s login stopped working"], tone: "danger" };
    case "login_expiring": {
      const at = e.detail?.expires_at as number | undefined;
      return { parts: [from, "'s sign-in ends", at ? ` on ${new Date(at).toLocaleDateString([], { weekday: "long", month: "short", day: "numeric" })}` : " soon"], tone: "warn" };
    }
    case "session":
      if (e.reason === "waiting") return { parts: e.account_from ? ["A session stopped at ", from, "'s usage limit"] : ["A session stopped at a usage limit"] };
      if (e.reason === "woken") return { parts: ["Woke a session that was stopped at a limit, now on ", to] };
      if (e.reason === "resumed")
        return e.detail?.woken === false
          ? { parts: ["A session stopped at a limit continued on ", to] }
          : { parts: ["A woken session continued on ", to] };
      return { parts: ["A session ", e.reason ?? "changed"] };
    case "paid_usage":
      return { parts: ["Every account is at a limit, and ", from, " is running on paid extra usage"], tone: "warn" };
    case "session_stuck":
      return { parts: ["A session woken on ", to, " hasn't continued"], tone: "warn" };
    case "claude_version": {
      const version = detail("version");
      return e.detail?.tested === false
        ? { parts: [`Claude Code ${version} is in use, not yet tested with cc-rotate`] }
        : { parts: [`Claude Code ${version} is in use`] };
    }
    case "warm_up":
      return { parts: ["Started ", from, "'s next 5-hour window"] };
    case "pool_exhausted":
      return { parts: ["No other account was ready to take over"], tone: "warn" };
    case "adopted_login":
      return { parts: ["Now using ", to, " after a /login in the profile"] };
    case "foreign_login":
      return { parts: ["The cc-rotate profile was signed in to an account that isn't in cc-rotate"], tone: "warn" };
    case "state":
      if (e.reason === "removed") return { parts: ["Removed ", nb(detail("label") || "an account")] };
      if (e.reason === "paused") return { parts: ["Paused ", from] };
      if (e.reason === "parked") return { parts: ["Resumed ", from] };
      if (e.reason === "order") return { parts: ["Changed the order"] };
      if (e.reason === "weekly_limit") {
        const value = detail("weekly_limit");
        const who = nb(detail("label") || labelOf(e.account_from));
        if (value === "yes") return { parts: ["Set ", who, " to have a weekly limit"] };
        if (value === "no") return { parts: ["Set ", who, " to no weekly limit"] };
        return { parts: ["Set ", who, " to detect its weekly limit from usage"] };
      }
      if (e.reason === "use_expiring") {
        const who = nb(detail("label") || labelOf(e.account_from));
        return e.detail?.use_expiring
          ? { parts: ["Set ", who, " to use its weekly budget before it resets"] }
          : { parts: ["Set ", who, " to never move ahead for its weekly budget"] };
      }
      if (e.reason === "keep_running") {
        const who = nb(detail("label") || labelOf(e.account_from));
        return e.detail?.keep_running
          ? { parts: ["Set ", who, " to keep its 5-hour window running"] }
          : { parts: ["Set ", who, " to start its 5-hour window only when it's used"] };
      }
      if (e.reason === "weekly_cap") {
        const value = e.detail?.weekly_cap as number | null | undefined;
        const who = nb(detail("label") || labelOf(e.account_from));
        return value == null
          ? { parts: ["Set ", who, " to stop at the weekly usage in Settings"] }
          : { parts: ["Set ", who, ` to stop at ${value}% weekly usage`] };
      }
      return { parts: [from, ` is now ${e.reason ?? "updated"}`] };
    case "error":
      return { parts: [detail("message") || "Something went wrong"], tone: "danger" };
    default:
      return { parts: [e.kind] };
  }
}

function dayName(ts: number): string {
  const day = new Date(ts).toDateString();
  const today = new Date();
  if (day === today.toDateString()) return "Today";
  if (day === new Date(today.getTime() - 86_400_000).toDateString()) return "Yesterday";
  return new Date(ts).toLocaleDateString([], { weekday: "long", month: "short", day: "numeric" });
}

/** The list items for `events`, newest first, with a heading for each day. */
export function activityItems(events: EventRow[], labelOf: (id: number | null) => string, limit: number): HTMLElement[] {
  if (events.length === 0) return [h("li", { class: "muted empty" }, "Swaps, sign-ins and problems show up here.")];
  const rows: HTMLElement[] = [];
  let lastDay = "";
  for (const e of events.slice(0, limit)) {
    const day = dayName(e.ts);
    if (day !== lastDay) {
      rows.push(h("li", { class: "day" }, day));
      lastDay = day;
    }
    const d = describe(e, labelOf);
    const time = new Date(e.ts);
    rows.push(
      h(
        "li",
        { class: d.tone ?? "" },
        h("time", { datetime: time.toISOString(), title: time.toLocaleString() }, time.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })),
        h("span", null, d.parts),
      ),
    );
  }
  return rows;
}
