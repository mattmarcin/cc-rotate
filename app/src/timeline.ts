// The overview's plan: one lane per account across the next hours, from the forecast's turns.
// A solid bar is the account in use, a tinted bar a planned turn, a hatched bar an account
// waiting for a limit to reset, and a band across every lane a stretch with no account left.
import type { AccountView, Snapshot } from "./api";
import { h, type Child } from "./dom";
import { clock, pct } from "./format";

const HOUR_MS = 3_600_000;
/** Hours the plan shows, from the start of the hour at least half an hour ago. */
export const PLAN_HOURS = 12;

export interface View {
  start: number;
  end: number;
  now: number;
}

export function planView(nowMs: number): View {
  // Now is never at the very edge, where its tag would be cut off.
  const start = new Date(nowMs - HOUR_MS / 2);
  start.setMinutes(0, 0, 0);
  return { start: start.getTime(), end: start.getTime() + PLAN_HOURS * HOUR_MS, now: nowMs };
}

export interface Segment {
  kind: "use" | "next" | "rest";
  /** Epoch ms. */
  from: number;
  to: number;
  text: string;
  title: string;
}

export interface Lane {
  segments: Segment[];
  /** Said across the lane when nothing is drawn in it. */
  note: string | null;
  tone: "danger" | null;
}

const time = (ms: number) => new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });

/** A time on the plan: "11:07 PM" within the next day, with the weekday further out. */
export const when = (ms: number, now: number) => (ms - now < 20 * HOUR_MS ? time(ms) : clock(ms));

/** "back at 3:25 AM", or "back Thu 9:00 AM" further out. */
const backText = (ms: number, now: number) => (ms - now < 20 * HOUR_MS ? `back at ${time(ms)}` : `back ${clock(ms)}`);

/** "raid2", "raid2 and raid1", "raid2, raid1 and raid4". */
const listOf = (names: string[]) => (names.length < 2 ? names.join("") : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`);

/** The lanes of every account, by id. `activeSince` is when the account in use was swapped in. */
export function lanes(snap: Snapshot, activeSince: number | null, view: View, labelOf: (id: number) => string): Map<number, Lane> {
  const out = new Map<number, Lane>(snap.accounts.map((a) => [a.id, { segments: [], note: null, tone: null }]));
  const add = (id: number, s: Segment) => {
    if (s.to > view.start && s.from < view.end && s.to > s.from) out.get(id)?.segments.push(s);
  };
  const plan = snap.forecast?.plan ?? [];
  // The plan starts with another account when the one in use hands over at once: at a limit,
  // or giving way to an account without a weekly limit.
  const activeTurn = plan[0]?.account_id === snap.active_id;

  plan.forEach((turn, i) => {
    const from = turn.from * 1000;
    const to = turn.to * 1000;
    const first = i === 0 && activeTurn;
    const open = to >= view.end;
    add(turn.account_id, {
      kind: first ? "use" : "next",
      from: first ? Math.min(from, activeSince ?? view.start) : from,
      to,
      text: first ? (open ? "In use" : `until about ${time(to)}`) : open ? `from ${time(from)}` : `${time(from)} to ${time(to)}`,
      title: first
        ? `${labelOf(turn.account_id)} is in use${open ? "" : ` until about ${when(to, view.now)}, at your pace`}`
        : `${labelOf(turn.account_id)} takes over at about ${when(from, view.now)}${open ? "" : ` until ${when(to, view.now)}`}`,
    });
    if (turn.rests_until && turn.rests_until > turn.to) {
      const back = turn.rests_until * 1000;
      add(turn.account_id, {
        kind: "rest",
        from: to,
        to: back,
        text: backText(back, view.now),
        title: `${labelOf(turn.account_id)} reaches a limit and is ready again at ${when(back, view.now)}`,
      });
    }
  });

  for (const a of snap.accounts) {
    const lane = out.get(a.id)!;
    // The account in use without a turn of its own (no pace yet, or handing over now): in use
    // up to now.
    if (a.id === snap.active_id && !activeTurn) {
      add(a.id, { kind: "use", from: activeSince ?? view.start, to: view.now, text: "In use", title: `${a.label} is in use` });
    }
    // An account at a limit now rests until it resets, before any turn the plan gives it.
    const back = a.available_at ? a.available_at * 1000 : null;
    const usable = a.state === "parked" || (a.state === "active" && !activeTurn);
    if (usable && back && back > view.now) {
      add(a.id, { kind: "rest", from: view.now, to: back, text: backText(back, view.now), title: `${a.label} is at a limit until ${when(back, view.now)}` });
    }
    lane.segments.sort((x, y) => x.from - y.from);
    if (a.state === "needs_login") {
      lane.note = "Signed out. Sign in again to use it.";
      lane.tone = "danger";
    } else if (a.state === "paused") {
      lane.note = "Paused. Resume it to use it again.";
    } else if (lane.segments.length === 0) {
      if (!a.eligible && a.state === "parked") lane.note = a.reason || "Not available";
      else if (snap.forecast) lane.note = `Not needed before ${time(view.end)}`;
      else if (a.state === "parked") lane.note = "Ready";
    }
  }
  return out;
}

/** Stretches with no account left, within the view: the plan's gaps and what follows its end. */
export function gaps(snap: Snapshot, view: View): { from: number; to: number }[] {
  const f = snap.forecast;
  if (!f || f.plan.length === 0) return [];
  const out: { from: number; to: number }[] = [];
  f.plan.forEach((t, i) => {
    const next = f.plan[i + 1];
    if (next && next.from > t.to) out.push({ from: t.to * 1000, to: next.from * 1000 });
  });
  const last = f.plan[f.plan.length - 1].to * 1000;
  // The plan stops short of the 24-hour horizon only when the pool is out at the end.
  if (last < view.end && f.runs_out_at !== null && last >= f.runs_out_at * 1000) out.push({ from: last, to: view.end });
  return out.filter((g) => g.to > view.start && g.from < view.end);
}

/** Where `ms` falls across the view, from 0 to 100. */
export const at = (ms: number, view: View) => (Math.max(view.start, Math.min(view.end, ms)) - view.start) / (view.end - view.start) * 100;

/** The hour labels above the lanes: every other hour, the day's name at midnight, now, and where no account is left. */
export function axis(snap: Snapshot, view: View): HTMLElement {
  const marks: HTMLElement[] = [];
  // Wide enough gaps are named in the axis, over the hour labels where the name goes.
  // Not right beside the tag for now, and not so near the end that the name is cut off.
  const named = gaps(snap, view).filter((g) => at(g.to, view) - at(g.from, view) > 9 && at(g.from, view) - at(view.now, view) > 7 && at(g.from, view) < 86);
  for (let t = view.start + HOUR_MS; t < view.end; t += HOUR_MS) {
    const d = new Date(t);
    const midnight = d.getHours() === 0;
    if (!midnight && d.getHours() % 2 !== 0) continue;
    // Too close to "now" to read beside it.
    if (Math.abs(t - view.now) < 0.45 * HOUR_MS) continue;
    if (named.some((g) => t > g.from - 0.5 * HOUR_MS && t < g.from + 1.6 * HOUR_MS)) continue;
    marks.push(
      h(
        "span",
        { class: midnight ? "day" : null, style: { left: `${at(t, view)}%` } },
        midnight ? d.toLocaleDateString([], { weekday: "short" }) : d.toLocaleTimeString([], { hour: "numeric" }),
      ),
    );
  }
  marks.push(h("span", { class: "now-tag", style: { left: `${at(view.now, view)}%` } }, "Now"));
  for (const g of named) marks.push(h("span", { class: "gap-tag", style: { left: `${at(g.from, view)}%` } }, "All at a limit"));
  return h("div", { class: "plan-axis", "aria-hidden": "true" }, marks);
}

/** One account's lane. */
export function laneEl(lane: Lane, view: View, color: string): HTMLElement {
  const width = (s: Segment) => at(s.to, view) - at(s.from, view);
  return h(
    "div",
    { class: "lane", style: { "--c": color } },
    lane.segments.map((s) => {
      const w = width(s);
      // The bar of the account in use starts before now: its label starts after the line for now.
      const pad = s.kind === "use" && s.from < view.now ? { paddingLeft: `calc(${at(view.now, view) - at(s.from, view)}% + 9px)` } : {};
      return h(
        "div",
        {
          class: `seg ${s.kind}`,
          style: { left: `${at(s.from, view)}%`, width: `${w}%`, ...pad },
          title: s.title,
          "data-long": s.text,
          // Where only part fits: the time it starts or ends.
          "data-short": s.text.replace(/^(back at|back|until about|from) /, "").split(" to ")[0],
        },
        s.text,
      );
    }),
    lane.note && h("div", { class: `lane-note ${lane.tone ?? ""}`.trim(), style: { left: `${at(view.now, view)}%` } }, lane.note),
  );
}

/** Give each bar under `root` the longest of its labels that fits, or none in a sliver. */
export function fitLabels(root: HTMLElement) {
  for (const seg of root.querySelectorAll<HTMLElement>(".seg")) {
    for (const text of [seg.dataset.long ?? "", seg.dataset.short ?? "", ""]) {
      seg.textContent = text;
      if (seg.scrollWidth <= seg.clientWidth) break;
    }
  }
}

/** The stretches with no account left and the hour lines behind every lane, and the line for now over them. */
export function overlay(snap: Snapshot, view: View): HTMLElement[] {
  const lines: HTMLElement[] = [];
  for (let t = view.start + HOUR_MS; t < view.end; t += HOUR_MS) lines.push(h("i", { class: "hour", style: { left: `${at(t, view)}%` } }));
  // The axis names them; the rows say what each account does meanwhile.
  const bands = gaps(snap, view).map((g) => h("div", { class: "plan-gap", style: { left: `${at(g.from, view)}%`, width: `${at(g.to, view) - at(g.from, view)}%` } }));
  return [
    h("div", { class: "grid-lines", "aria-hidden": "true" }, bands, lines),
    h("div", { class: "plan-over", "aria-hidden": "true" }, h("i", { class: "now-line", style: { left: `${at(view.now, view)}%` } })),
  ];
}

/** The account that takes over next: the plan's, or the one a swap now would pick. */
export function nextUp(snap: Snapshot): number | null {
  const later = snap.forecast?.plan.find((t) => t.account_id !== snap.active_id);
  return later?.account_id ?? snap.next?.account_id ?? null;
}

/** The overview's headline and the sentences under it. */
export function summary(snap: Snapshot, labelOf: (id: number) => string): { title: string; story: Child[]; tone: "ok" | "warn" | "idle" } {
  const active = snap.accounts.find((a) => a.id === snap.active_id);
  const f = snap.forecast;
  const b = (text: string) => h("b", null, text);
  const now = Date.now();
  if (snap.pool.exhausted) {
    const next = snap.pool.next_available_at;
    return {
      title: "Every account is at a limit",
      story: next ? ["The first one frees up at ", b(when(next * 1000, now)), ". Sessions stopped at a limit pick up again then."] : ["None says when it resets."],
      tone: "warn",
    };
  }
  if (!active) {
    return {
      title: "No account in use",
      story: ["Start a session with ", b("cc-rotate"), ", or pick an account below and use it now."],
      tone: "idle",
    };
  }
  const five = active.windows.find((w) => w.kind === "session" && w.model === null);
  const left = five ? `${pct(Math.max(0, 100 - five.pct))} of its 5-hour window is left` : "its 5-hour window hasn't started";
  if (!f || f.plan.length === 0) {
    return {
      title: `Using ${active.label}`,
      story: [b(active.label), ` is in use, and ${left}. The plan fills in from your pace once a session is working.`],
      tone: "idle",
    };
  }
  const first = f.plan[0];
  const out = f.runs_out_at;
  // The turns before the pool runs out, if it does.
  const before = out ? f.plan.filter((t) => t.to <= out) : f.plan;
  const ownTurn = first.account_id === active.id;
  // The other accounts that take over, in the order they first do.
  const names = [...new Set(before.map((t) => t.account_id))].filter((id) => id !== active.id).map(labelOf);
  const opening: Child[] = ownTurn
    ? !out && f.plan.length === 1
      ? [b(active.label), " has room for the next 24 hours at your pace."]
      : ["At your pace, ", b(active.label), first.at_limit ? " reaches its swap point around " : " gives way around ", b(when(first.to * 1000, now)), "."]
    : [
        b(active.label),
        active.windows.some((w) => w.applies && w.pct >= w.threshold) ? " is at its swap point, so " : " gives way to an account without a weekly limit: ",
        b(labelOf(first.account_id)),
        " takes over now.",
      ];
  // Who comes after the opening's accounts.
  const rest = ownTurn ? names : names.slice(1);
  const handover: Child[] = [opening, rest.length === 0 ? "" : rest.length === 1 ? ` Then ${rest[0]} takes over.` : ` Then ${listOf(rest)} take turns.`];
  if (f.runs_out_at) {
    return {
      title: `Runs out around ${when(f.runs_out_at * 1000, now)}`,
      story: [
        handover,
        " Every account reaches a limit then",
        f.free_again_at ? [", and the first one frees up at ", b(when(f.free_again_at * 1000, now)), ". Sessions stopped at a limit pick up again then."] : ".",
      ],
      tone: "warn",
    };
  }
  return { title: "Covered for the next 24 hours", story: handover, tone: "ok" };
}

/** The account's line under its name in the plan. */
export function stateLine(a: AccountView, isNext: boolean): string {
  const five = a.windows.find((w) => w.kind === "session" && w.model === null);
  const week = a.windows.find((w) => w.kind === "weekly" && w.model === null);
  switch (a.state) {
    case "active":
      return five ? `In use, ${pct(five.pct)} of 5 hours used` : "In use";
    case "paused":
      return "Paused";
    case "needs_login":
      return "Signed out";
    case "parked":
      if (!a.eligible) return a.available_at ? `Back ${when(a.available_at * 1000, Date.now())}` : "Not available";
      if (isNext) return "Next";
      return week ? `Ready, ${pct(week.pct)} of its week used` : "Ready";
  }
}
