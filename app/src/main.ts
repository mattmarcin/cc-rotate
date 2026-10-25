import "@fontsource-variable/schibsted-grotesk";
import "./styles.css";

import {
  api,
  onLoginProgress,
  onStoreChanged,
  type AccountHistory,
  type AccountView,
  type CheckOutcome,
  type EventRow,
  type ExtraUsage,
  type LoginProgress,
  type Snapshot,
  type WeeklyLimitSetting,
  type WindowUsage,
} from "./api";
import { UsageChart, colorFor, type Metric } from "./chart";
import { h, replace, type Child } from "./dom";
import { ago, clock, ordinal, pct, plural, until } from "./format";

interface UiState {
  snap: Snapshot | null;
  events: EventRow[];
  showRoutine: boolean;
  metric: Metric;
  rangeDays: 1 | 7;
  busy: boolean;
  /** A row is being dragged; renders wait until it is dropped. */
  dragging: boolean;
  /** The account whose order handle should have focus after the next render. */
  focusGrip: number | null;
}

const state: UiState = {
  snap: null,
  events: [],
  showRoutine: false,
  metric: "weekly",
  rangeDays: 7,
  busy: false,
  dragging: false,
  focusGrip: null,
};

const ROUTINE = new Set(["sync", "swap_skipped", "claude_version", "warm_up"]);

// ---- layout ----------------------------------------------------------------------------

const root = document.getElementById("app")!;
const topActions = h("div", { class: "top-actions" });
const banner = h("div", { class: "banner", hidden: true });
const bank = h("div", { class: "bank", role: "list", "aria-label": "Accounts" });
const summary = h("div", { class: "summary" });
const accountsBody = h("div", { class: "accounts" });
const chartControls = h("div", { class: "chart-controls" });
const chartLegend = h("div", { class: "legend" });
const chartHost = h("div", { class: "chart" });
const chartEmpty = h("p", { class: "chart-empty", hidden: true }, "Usage appears here once a session runs in the cc-rotate profile.");
const activity = h("ol", { class: "activity" });
const routineToggle = h("input", {
  type: "checkbox",
  id: "routine",
  onchange: (e: Event) => {
    state.showRoutine = (e.target as HTMLInputElement).checked;
    renderActivity();
  },
});
const toast = h("div", { class: "toast", role: "status", "aria-live": "polite" });

root.append(
  h(
    "header",
    { class: "top" },
    h("div", { class: "brand" }, h("img", { src: "/icon.svg", alt: "", width: 22, height: 22 }), "cc-rotate"),
    topActions,
  ),
  banner,
  h("section", { class: "overview" }, bank, summary),
  h(
    "main",
    { class: "content" },
    h(
      "div",
      { class: "main-col" },
      h("section", { class: "panel" }, h("h2", null, "Accounts"), accountsBody),
      h(
        "section",
        { class: "panel" },
        h("div", { class: "panel-head" }, h("h2", null, "Usage over time"), chartControls),
        chartLegend,
        h("div", { class: "chart-wrap" }, chartHost, chartEmpty),
      ),
    ),
    h(
      "aside",
      { class: "rail" },
      h(
        "div",
        { class: "panel-head" },
        h("h2", null, "Activity"),
        h("label", { class: "check", for: "routine" }, routineToggle, "Show all"),
      ),
      activity,
    ),
  ),
  toast,
);

const chart = new UsageChart(chartHost);

// ---- helpers ---------------------------------------------------------------------------

let toastTimer = 0;
function say(message: string, kind: "info" | "error" = "info") {
  toast.textContent = message;
  toast.className = `toast show ${kind}`;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (toast.className = "toast"), kind === "error" ? 7000 : 3500);
}

async function act<T>(work: () => Promise<T>, done?: (result: T) => string | null) {
  if (state.busy) return;
  state.busy = true;
  document.body.classList.add("busy");
  try {
    const result = await work();
    const message = done?.(result);
    if (message) say(message);
  } catch (e) {
    say(String(e), "error");
  } finally {
    state.busy = false;
    document.body.classList.remove("busy");
    await refresh();
  }
}

const labelOf = (id: number | null) => state.snap?.accounts.find((a) => a.id === id)?.label ?? "another account";

/** Colors stay with an account when the order changes. */
let colorOf = (_id: number) => "";

/** The 5-hour and weekly windows on all models, when the latest reading has them. */
const fiveOf = (a: AccountView) => a.windows.find((w) => w.kind === "session" && w.model === null);
const weekOf = (a: AccountView) => a.windows.find((w) => w.kind === "weekly" && w.model === null);
/** Windows limited to one model, or of a kind cc-rotate does not know. */
const otherWindows = (a: AccountView) => a.windows.filter((w) => w.model !== null || w.kind === "other");
const blocking = (a: AccountView) => a.windows.some((w) => w.applies && w.pct >= w.threshold);
/** No weekly limit: the 5-hour window is what runs out. */
const noWeekly = (a: AccountView) => a.weekly.effective === "none";

/** The other profile a reading came from, by its short name ("work" for `.claude-work`). */
function readIn(source: string | null): string | null {
  if (!source || source === "rotate") return null;
  if (source === ".claude") return "main";
  return source.replace(/^\.claude-/, "").replace(/^\./, "");
}

/** "updated 5m ago", "checked 5m ago", or "updated 5m ago from work" for a reading another profile reported. */
function readText(a: AccountView): string {
  if (!a.last_sample_at) return "no data yet";
  if (a.last_sample_source === "check") return `checked ${ago(a.last_sample_at)}`;
  const other = readIn(a.last_sample_source);
  return `updated ${ago(a.last_sample_at)}${other ? ` from ${other}` : ""}`;
}

function readTitle(a: AccountView): string {
  if (!a.last_sample_at) return "";
  const at = new Date(a.last_sample_at).toLocaleString();
  if (a.last_sample_source === "check") return `${at}, from a usage check`;
  const other = readIn(a.last_sample_source);
  return other ? `${at}, from the ${other} profile` : at;
}

/** A toast for the outcome of a usage check the user asked for. */
function checkMessage(label: string, r: CheckOutcome): string {
  switch (r.outcome) {
    case "read":
      return `Checked ${label}`;
    case "rate_limited":
      return `Too soon to check ${label}. Anthropic asked to wait ${until(r.until / 1000)}.`;
    case "skipped":
      return `Didn't check ${label}: ${r.why}`;
    case "login_rejected":
      return `${label}'s login was rejected. Sign in again.`;
    case "failed":
      return `Couldn't check ${label}: ${r.why}`;
  }
}

/** The usage check facts in the account dialog. */
function checkText(a: AccountView): string {
  const c = a.usage_check;
  if (a.state === "active") return "not needed while in use";
  if (c.held_back) return "on hold until a check confirms its login";
  if (c.problem) return c.problem;
  const read = c.last_read_at ? `checked ${ago(c.last_read_at)}` : "not checked yet";
  return c.not_before && c.not_before > Date.now() ? `${read}, next after ${clock(c.not_before)}` : read;
}

function warmText(a: AccountView): string {
  const c = a.usage_check;
  if (c.warm_problem) return `last try failed: ${c.warm_problem}`;
  return c.warmed_at ? `last started its window ${ago(c.warmed_at)}` : "hasn't started a window yet";
}

const WEEKLY_TEXT = { none: "none found", unknown: "not known yet", has: "has one" } as const;

/** An amount in minor units of `currency` (cents for USD), as ccr-core's `extra_usage::money` writes it. */
function money(minor: number, currency: string | null): string {
  const code = (currency ?? "USD").toUpperCase();
  const whole = ["JPY", "KRW", "CLP", "ISK", "VND"].includes(code);
  const amount = whole ? String(Math.round(minor)) : (minor / 100).toFixed(2);
  return code === "USD" ? `$${amount}` : `${amount} ${code}`;
}

/** "$12.40", or "$123" from $100 up. */
function dollars(usd: number): string {
  return usd >= 100 ? `$${Math.round(usd)}` : `$${usd.toFixed(2)}`;
}

/** The account's extra usage, as ccr-core's `extra_usage::describe` writes it. */
function extraText(e: ExtraUsage): string {
  if (!e.enabled) {
    return e.used !== null && e.monthly_limit !== null && e.monthly_limit > 0 && e.used >= e.monthly_limit
      ? `off, its ${money(e.monthly_limit, e.currency)} monthly limit is used up`
      : "off";
  }
  if (e.used !== null && e.monthly_limit !== null) return `on, ${money(e.used, e.currency)} of ${money(e.monthly_limit, e.currency)} used this month`;
  if (e.used !== null) return `on, ${money(e.used, e.currency)} used this month`;
  return "on";
}

/** "API value" in the account dialog: what the last weeks' use would have cost at API prices. */
function valueText(hist: AccountHistory): string {
  const counted = hist.weeks.filter((w) => w.api_value_usd !== null);
  if (counted.length === 0) return "nothing counted yet";
  const total = counted.reduce((sum, w) => sum + (w.api_value_usd ?? 0), 0);
  const weeks = hist.weeks.length;
  return `${dollars(total)} in the last ${weeks === 1 ? "week" : `${weeks} weeks`}`;
}

/** Weeks "Recent weeks" shows in the account dialog. */
const RECENT_WEEKS = 4;

/**
 * "Recent weeks" in the account dialog, newest first: the highest usage of each weekly window
 * that closed, or for an account without a weekly limit, how many of each week's 5-hour windows
 * reached the limit. A week cc-rotate saw little of is dimmed, and its tooltip says why.
 */
function recentWeeks(hist: AccountHistory): Child[] {
  const day = (s: number) => new Date(s * 1000).toLocaleDateString([], { month: "short", day: "numeric" });
  const few = "Few readings, so it may have been higher.";
  const list = (items: { text: string; title: string; dim: boolean }[], note: string): Child[] => [
    items.map((it, i) => [i > 0 && ", ", h("span", { class: it.dim ? "muted" : null, title: it.title }, it.text)]),
    h("small", { class: "muted" }, note),
  ];
  if (hist.kind === "weekly") {
    const windows = hist.weekly.slice(0, RECENT_WEEKS);
    if (windows.length === 0) return ["no data yet"];
    return list(
      windows.map((w) => ({
        text: pct(w.peak_pct),
        dim: w.thin,
        title: `Week to ${day(w.resets_at)}. Highest weekly usage: ${pct(w.peak_pct)}.${w.thin ? ` ${few}` : ""}`,
      })),
      "Highest weekly usage, newest first",
    );
  }
  const weeks = hist.weeks.filter((w) => !w.in_progress).slice(0, RECENT_WEEKS);
  if (weeks.every((w) => w.windows_used === null)) return ["no data yet"];
  return list(
    weeks.map((w) =>
      w.windows_used === null
        ? { text: "-", dim: true, title: `Week of ${day(w.starts_at)}. No data.` }
        : {
            text: `${w.windows_at_limit ?? 0} of ${w.windows_used}`,
            dim: w.thin,
            title: `Week of ${day(w.starts_at)}. 5-hour windows at the limit: ${w.windows_at_limit ?? 0} of ${w.windows_used}.${w.thin ? ` ${few}` : ""}`,
          },
    ),
    "5-hour windows at the limit, newest first",
  );
}

function level(used: number, threshold: number): "ok" | "warn" | "over" {
  if (used >= threshold) return "over";
  if (used >= threshold - 18) return "warn";
  return "ok";
}

function stateText(a: AccountView): string {
  switch (a.state) {
    case "active":
      return "In use";
    case "parked":
      return a.eligible ? "Ready" : "Resting";
    case "paused":
      return "Paused";
    case "needs_login":
      return "Signed out";
  }
}

// ---- top bar and banner ----------------------------------------------------------------

function renderTop() {
  const snap = state.snap;
  const hasAccounts = !!snap && snap.accounts.length > 0;
  replace(
    topActions,
    h("button", { class: "btn quiet", onclick: openSettings }, "Settings"),
    hasAccounts &&
      h(
        "button",
        {
          class: "btn",
          disabled: !snap!.pool.eligible,
          title: snap!.pool.eligible ? "Swap to the next account in line" : "No other account is ready",
          onclick: () =>
            act(
              () => api.swapNow(null),
              (r) =>
                r.outcome === "swapped"
                  ? `Swapped to ${labelOf(r.to)}`
                  : r.outcome === "pool_exhausted"
                    ? "No other account is ready"
                    : `No swap: ${r.why}`,
            ),
        },
        "Swap now",
      ),
    h("button", { class: "btn primary", onclick: () => openLogin(null) }, "Add account"),
  );

  const needs = snap?.accounts.filter((a) => a.state === "needs_login") ?? [];
  if (snap?.pool.swap_unsettled) {
    banner.hidden = false;
    banner.className = "banner warn";
    replace(
      banner,
      h(
        "span",
        null,
        "A swap was interrupted. Swaps are on hold until cc-rotate confirms which account the profile is signed in to. Your sessions keep working.",
      ),
    );
  } else if (needs.length > 0) {
    banner.hidden = false;
    banner.className = "banner danger";
    replace(
      banner,
      h(
        "span",
        null,
        needs.length === 1
          ? `${needs[0].label} is signed out. Sign in again to use it.`
          : `${needs.map((a) => a.label).join(", ")} are signed out. Sign in again to use them.`,
      ),
      h("button", { class: "btn small", onclick: () => openLogin(needs[0]) }, `Sign in to ${needs[0].label}`),
    );
  } else if (snap?.pool.exhausted && snap.accounts.length > 0) {
    banner.hidden = false;
    banner.className = "banner warn";
    const next = snap.pool.next_available_at;
    replace(
      banner,
      h(
        "span",
        null,
        "No account is ready to take over. ",
        next ? `The next one frees up in ${until(next)}.` : "",
      ),
    );
  } else if (snap && snap.pool.sign_ins_ending.length > 0) {
    // The sign-in that ends first, first.
    const ending = snap.accounts
      .filter((a) => a.sign_in_ends_soon)
      .sort((x, y) => (x.refresh_expires_at ?? 0) - (y.refresh_expires_at ?? 0));
    const first = ending[0];
    banner.hidden = false;
    banner.className = "banner warn";
    replace(
      banner,
      h(
        "span",
        null,
        ending.length === 1 && first.refresh_expires_at
          ? `${first.label}'s sign-in ends in ${until(first.refresh_expires_at / 1000)}. Sign in again to keep it in rotation.`
          : `${ending.map((a) => a.label).join(", ")} need a new sign-in soon. Sign in again to keep them in rotation.`,
      ),
      h("button", { class: "btn small", onclick: () => openLogin(first) }, `Sign in to ${first.label}`),
    );
  } else {
    banner.hidden = true;
  }
}

// ---- overview: the bank and the pool summary --------------------------------------------

function renderOverview() {
  const snap = state.snap;
  if (!snap || snap.accounts.length === 0) {
    replace(
      bank,
      h(
        "div",
        { class: "onboard" },
        h("h1", null, "Add your first account"),
        h(
          "p",
          null,
          "Sign in to each of your Claude accounts once. cc-rotate keeps the logins encrypted on this computer and swaps the cc-rotate profile to another account when one nears its limits.",
        ),
        h("button", { class: "btn primary", onclick: () => openLogin(null) }, "Add account"),
      ),
    );
    replace(summary);
    return;
  }
  replace(
    bank,
    snap.accounts.map((a) => {
      const five = fiveOf(a);
      const week = weekOf(a);
      // The tall tank is the budget that runs out first: the weekly one, or the 5-hour one for
      // an account without a weekly limit.
      const tankWindow = noWeekly(a) ? five : week;
      const tankLeft = Math.max(0, 100 - (tankWindow?.pct ?? 0));
      const fiveLeft = Math.max(0, 100 - (five?.pct ?? 0));
      const tone =
        a.state === "needs_login"
          ? "dead"
          : a.state === "paused"
            ? "paused"
            : blocking(a)
              ? "over"
              : a.state === "active"
                ? "active"
                : level(tankWindow?.pct ?? 0, tankWindow?.threshold ?? 100);
      const title = [
        `${a.label}${a.email ? ` (${a.email})` : ""}`,
        [stateText(a), a.plan].filter(Boolean).join(", "),
        noWeekly(a) ? "No weekly limit" : `Weekly: ${week ? `${pct(week.pct)} used` : "no data"}`,
        `5-hour: ${five ? `${pct(five.pct)} used` : "no data"}`,
        ...otherWindows(a).map((w) => `${w.label}: ${pct(w.pct)} used`),
      ].join("\n");
      const sub =
        a.state === "needs_login"
          ? "Signed out"
          : a.state === "paused"
            ? "Paused"
            : !a.last_sample_at
              ? "No data"
              : noWeekly(a)
                ? `5h ${pct(tankLeft)} left`
                : `${pct(tankLeft)} left`;
      return h(
        "div",
        { class: `cell ${tone}${noWeekly(a) ? " no-weekly" : ""}`, role: "listitem", title },
        h("div", { class: "tank" }, h("div", { class: "fill", style: { height: `${a.state === "needs_login" ? 0 : tankLeft}%` } })),
        h("div", { class: "five", "aria-hidden": "true" }, h("span", { style: { width: `${a.state === "needs_login" ? 0 : fiveLeft}%` } })),
        h("div", { class: "cell-label" }, a.label),
        h("div", { class: "cell-sub tnum" }, sub),
      );
    }),
  );

  const active = snap.accounts.find((a) => a.id === snap.active_id);
  const p = snap.pool;
  const unavailable = snap.accounts.length - p.usable;
  const swapEvent = state.events.find((e) => e.kind === "swap" && e.account_to === snap.active_id);
  const headline =
    p.weekly_limited > 0
      ? `Weekly usage left: ${p.weekly_accounts_left.toFixed(1)} of ${p.weekly_limited} accounts`
      : `${plural(p.usable, "account")}, none with a weekly limit`;
  replace(
    summary,
    h("p", { class: "headline" }, headline),
    h(
      "dl",
      { class: "facts" },
      h("dt", null, "Using"),
      h("dd", null, active ? active.label : "None", active && swapEvent ? h("span", { class: "muted" }, ` since ${clock(swapEvent.ts)}`) : null),
      h("dt", null, "Next"),
      h("dd", null, snap.next ? [snap.next.label, h("span", { class: "muted" }, ` - ${snap.next.reason}`)] : "None ready"),
      snap.forecast && h("dt", null, "At this pace"),
      snap.forecast &&
        h(
          "dd",
          { class: snap.forecast.runs_out_at ? "warn" : null },
          snap.forecast.runs_out_at
            ? [
                `all at a limit by ${clock(snap.forecast.runs_out_at * 1000)}`,
                snap.forecast.free_again_at
                  ? h("span", { class: "muted" }, `, one frees up at ${clock(snap.forecast.free_again_at * 1000)}`)
                  : null,
              ]
            : "room for the next 24 hours",
        ),
      p.weekly_limited > 0 && p.no_weekly_limit > 0 && h("dt", null, "No weekly limit"),
      p.weekly_limited > 0 && p.no_weekly_limit > 0 && h("dd", null, plural(p.no_weekly_limit, "account")),
      p.sessions_waiting > 0 && h("dt", null, "At a limit"),
      p.sessions_waiting > 0 &&
        h("dd", { class: "warn" }, plural(p.sessions_waiting, "session"), h("span", { class: "muted" }, " waiting for an account with room")),
      unavailable > 0 && h("dt", null, "Unavailable"),
      unavailable > 0 && h("dd", null, `${unavailable}`, h("span", { class: "muted" }, " paused or signed out")),
      h("dt", null, "Swaps"),
      h("dd", null, String(p.swaps_24h), h("span", { class: "muted" }, " in the last 24 hours")),
    ),
  );
}

// ---- accounts table --------------------------------------------------------------------

function meter(w: WindowUsage | undefined, live: boolean) {
  const used = w?.pct ?? 0;
  const lvl = level(used, w?.threshold ?? 100);
  // A usage report names no reset time for a window that has not started (nothing used yet).
  // No window at all is not 0% used: nothing was reported for it, so no number is shown.
  const reset = !w
    ? live
      ? "not reported yet"
      : "no data yet"
    : w.resets_at && w.resets_at * 1000 > Date.now()
      ? `resets in ${until(w.resets_at)}`
      : w.resets_at === null
        ? "not started"
        : live
          ? "has reset"
          : "no data yet";
  return h(
    "div",
    { class: `meter ${lvl}` },
    h(
      "div",
      { class: "bar", role: "img", "aria-label": w ? `${pct(used)} used` : reset },
      h("span", { style: { width: `${Math.min(100, used)}%` } }),
    ),
    h("div", { class: "meter-text" }, h("strong", { class: "tnum" }, w ? pct(used) : "-"), h("span", { class: "muted" }, reset)),
  );
}

function weeklyCell(a: AccountView) {
  const week = weekOf(a);
  const main =
    week || !noWeekly(a)
      ? meter(week, !!a.last_sample_at)
      : h(
          "div",
          { class: "meter-text none" },
          h("strong", null, "No weekly limit"),
          h("span", { class: "muted" }, a.weekly.setting === "no" ? "set by you" : "detected"),
        );
  const others = otherWindows(a);
  return [
    main,
    others.length > 0 &&
      h(
        "ul",
        { class: "scoped" },
        others.map((w) =>
          h(
            "li",
            { class: `${w.applies ? "" : "muted"} ${w.applies && w.pct >= w.threshold ? "over" : ""}`.trim() || null },
            `${w.label[0].toUpperCase()}${w.label.slice(1)} ${pct(w.pct)}`,
            !w.applies && h("span", null, ", not for the model in use"),
          ),
        ),
      ),
  ];
}

const GRIP = () => {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 10 16");
  svg.setAttribute("width", "10");
  svg.setAttribute("height", "16");
  svg.setAttribute("aria-hidden", "true");
  for (const [cx, cy] of [
    [2.5, 3],
    [7.5, 3],
    [2.5, 8],
    [7.5, 8],
    [2.5, 13],
    [7.5, 13],
  ]) {
    const dot = document.createElementNS("http://www.w3.org/2000/svg", "circle");
    dot.setAttribute("cx", String(cx));
    dot.setAttribute("cy", String(cy));
    dot.setAttribute("r", "1.4");
    svg.append(dot);
  }
  return svg;
};

/** Order saves run one after another, so quick key presses are never dropped. */
let orderSaves = Promise.resolve();
let pendingOrderSaves = 0;

/** Save a new order: optimistic in the UI, then written to the store. */
function saveOrder(ids: number[], moved: AccountView) {
  const snap = state.snap;
  if (!snap) return;
  const before = snap.accounts.map((a) => a.id);
  if (ids.every((id, i) => id === before[i])) return;
  snap.accounts.sort((x, y) => ids.indexOf(x.id) - ids.indexOf(y.id));
  renderOverview();
  renderAccounts();
  pendingOrderSaves += 1;
  orderSaves = orderSaves
    .then(() => api.setOrder(ids))
    .then(() => say(`${moved.label} is now ${ordinal(ids.indexOf(moved.id) + 1)} in your order`))
    .catch((e) => say(String(e), "error"))
    .finally(() => {
      pendingOrderSaves -= 1;
      // Refresh once the last save landed, so an earlier one never shows a stale order.
      if (pendingOrderSaves === 0) refresh();
    });
}

function moveBy(a: AccountView, delta: number) {
  const ids = state.snap?.accounts.map((x) => x.id) ?? [];
  const from = ids.indexOf(a.id);
  const to = Math.max(0, Math.min(ids.length - 1, from + delta));
  if (from < 0 || to === from) return;
  ids.splice(from, 1);
  ids.splice(to, 0, a.id);
  state.focusGrip = a.id;
  saveOrder(ids, a);
}

/** Drag a row by its handle. Pointer events rather than HTML drag and drop, which the desktop
 * webviews handle differently (or hand to the window as a file drop). */
function startDrag(e: PointerEvent, a: AccountView) {
  if (e.button !== 0 || state.busy) return;
  const grip = e.currentTarget as HTMLElement;
  const row = grip.closest("tr")!;
  const body = row.parentElement!;
  const rows = [...body.children] as HTMLTableRowElement[];
  if (rows.length < 2) return;
  e.preventDefault();
  const from = rows.indexOf(row);
  const boxes = rows.map((r) => r.getBoundingClientRect());
  const height = boxes[from].height;
  const startY = e.clientY;
  let to = from;
  let moved = false;

  const update = (clientY: number) => {
    const min = boxes[0].top - boxes[from].top;
    const max = boxes[boxes.length - 1].bottom - boxes[from].bottom;
    const dy = Math.max(min, Math.min(max, clientY - startY));
    if (!moved && Math.abs(dy) < 3) return;
    if (!moved) {
      moved = true;
      state.dragging = true;
      row.classList.add("dragging");
      body.classList.add("sorting");
      document.body.classList.add("grabbing");
    }
    row.style.transform = `translateY(${dy}px)`;
    const center = boxes[from].top + height / 2 + dy;
    to = from;
    boxes.forEach((b, i) => {
      const mid = b.top + b.height / 2;
      if (i < from && center < mid) to = Math.min(to, i);
      if (i > from && center > mid) to = Math.max(to, i);
    });
    rows.forEach((r, i) => {
      if (i === from) return;
      const shift = from < to && i > from && i <= to ? -height : to < from && i >= to && i < from ? height : 0;
      r.style.transform = shift ? `translateY(${shift}px)` : "";
    });
  };
  const finish = (commit: boolean) => {
    grip.removeEventListener("pointermove", onMove);
    grip.removeEventListener("pointerup", onUp);
    grip.removeEventListener("pointercancel", onCancel);
    window.removeEventListener("keydown", onKey, true);
    rows.forEach((r) => (r.style.transform = ""));
    row.classList.remove("dragging");
    body.classList.remove("sorting");
    document.body.classList.remove("grabbing");
    const wasDragging = state.dragging;
    state.dragging = false;
    if (commit && moved && to !== from) {
      const ids = rows.map((r) => Number(r.dataset.id));
      ids.splice(from, 1);
      ids.splice(to, 0, a.id);
      saveOrder(ids, a);
    } else if (wasDragging) {
      renderAccounts();
    }
  };
  const onMove = (ev: PointerEvent) => update(ev.clientY);
  const onUp = () => finish(true);
  const onCancel = () => finish(false);
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      ev.stopPropagation();
      finish(false);
    }
  };
  grip.setPointerCapture(e.pointerId);
  grip.addEventListener("pointermove", onMove);
  grip.addEventListener("pointerup", onUp);
  grip.addEventListener("pointercancel", onCancel);
  window.addEventListener("keydown", onKey, true);
}

function gripKeys(e: KeyboardEvent, a: AccountView) {
  const count = state.snap?.accounts.length ?? 0;
  const at = state.snap?.accounts.findIndex((x) => x.id === a.id) ?? 0;
  const delta =
    e.key === "ArrowUp" ? -1 : e.key === "ArrowDown" ? 1 : e.key === "Home" ? -at : e.key === "End" ? count - 1 - at : 0;
  if (delta === 0) return;
  e.preventDefault();
  moveBy(a, delta);
}

function renderAccounts() {
  if (state.dragging) return;
  const snap = state.snap;
  if (!snap || snap.accounts.length === 0) {
    replace(accountsBody, h("p", { class: "muted empty" }, "No accounts yet."));
    return;
  }
  const count = snap.accounts.length;
  const active = document.activeElement as HTMLElement | null;
  const hadFocus = active && accountsBody.contains(active) ? active.dataset.grip : undefined;
  const rows = snap.accounts.map((a, i) => {
    const actions = [];
    if (a.state === "parked")
      actions.push(h("button", { class: "link", onclick: () => act(() => api.swapNow(a.id), () => `Swapped to ${a.label}`) }, "Use now"));
    if (a.state === "paused")
      actions.push(h("button", { class: "link", onclick: () => act(() => api.setPaused(a.id, false), () => `Resumed ${a.label}`) }, "Resume"));
    if (a.state === "needs_login") actions.push(h("button", { class: "link", onclick: () => openLogin(a) }, "Sign in again"));
    actions.push(h("button", { class: "link quiet-link", onclick: () => openManage(a), "aria-label": `More options for ${a.label}` }, "More"));

    const grip = h(
      "button",
      {
        class: "grip",
        type: "button",
        title: "Drag to reorder",
        "aria-label": `${a.label}, ${ordinal(i + 1)} of ${count} in your order. Use the up and down arrow keys to move it.`,
        "data-grip": a.id,
        onpointerdown: (e: PointerEvent) => startDrag(e, a),
        onkeydown: (e: KeyboardEvent) => gripKeys(e, a),
      },
      GRIP(),
    );

    return h(
      "tr",
      { class: a.state === "active" ? "is-active" : "", "data-id": a.id, title: a.reason ? `${a.rank ? `${ordinal(a.rank)} pick: ` : ""}${a.reason}` : null },
      h("td", { class: "order" }, grip),
      h(
        "td",
        null,
        h(
          "div",
          { class: "who" },
          h("span", { class: "swatch", style: { background: colorOf(a.id) }, "aria-hidden": "true" }),
          h(
            "div",
            null,
            h("div", { class: "who-label" }, a.label, a.plan && h("span", { class: "plan" }, a.plan)),
            h("div", { class: "muted who-email" }, a.email ?? ""),
          ),
        ),
      ),
      h(
        "td",
        null,
        h("span", { class: `badge ${a.state}${a.state === "parked" && !a.eligible ? " resting" : ""}` }, stateText(a)),
        h(
          "div",
          { class: "muted updated", title: readTitle(a) },
          readText(a),
        ),
        a.sign_in_ends_soon &&
          a.refresh_expires_at &&
          h(
            "div",
            { class: "updated sign-in-ends", title: new Date(a.refresh_expires_at).toLocaleString() },
            `sign-in ends in ${until(a.refresh_expires_at / 1000)}`,
          ),
      ),
      h("td", null, meter(fiveOf(a), !!a.last_sample_at)),
      h("td", null, weeklyCell(a)),
      h("td", { class: "actions" }, actions),
    );
  });
  replace(
    accountsBody,
    h(
      "table",
      null,
      h(
        "thead",
        null,
        h(
          "tr",
          null,
          h("th", { class: "order" }, h("span", { class: "sr" }, "Order")),
          h("th", null, "Account"),
          h("th", null, "State"),
          h("th", null, "5-hour limit"),
          h("th", null, "Weekly limit"),
          h("th", null, h("span", { class: "sr" }, "Actions")),
        ),
      ),
      h("tbody", null, rows),
    ),
    h(
      "p",
      { class: "muted table-note" },
      [
        snap.settings.strict_order
          ? snap.settings.prefer_no_weekly_limit
            ? "Used top to bottom, accounts without a weekly limit first."
            : "Used top to bottom."
          : snap.settings.prefer_no_weekly_limit
            ? "Accounts without a weekly limit first. Within each group, the account in use stays until a limit, then accounts whose 5-hour window is already running, then top to bottom."
            : "The account in use stays until a limit, then accounts whose 5-hour window is already running, then top to bottom.",
        snap.settings.use_expiring_weekly ? "An account whose weekly budget would go unused goes first." : null,
        "Drag to reorder.",
      ]
        .filter(Boolean)
        .join(" "),
    ),
  );
  // Keep keyboard focus on the same account's handle across re-renders.
  const keep = state.focusGrip ?? (hadFocus ? Number(hadFocus) : null);
  if (keep !== null) {
    accountsBody.querySelector<HTMLElement>(`[data-grip="${keep}"]`)?.focus();
    state.focusGrip = null;
  }
}

// ---- chart -----------------------------------------------------------------------------

async function renderChart() {
  const snap = state.snap;
  const segmented = (options: [string, string][], current: string, pick: (v: string) => void, label: string) =>
    h(
      "div",
      { class: "segmented", role: "group", "aria-label": label },
      options.map(([value, text]) =>
        h("button", { class: value === current ? "on" : "", "aria-pressed": String(value === current), onclick: () => pick(value) }, text),
      ),
    );
  replace(
    chartControls,
    segmented([["weekly", "Weekly"], ["five_hour", "5-hour"]], state.metric, (v) => ((state.metric = v as Metric), renderChart()), "Limit"),
    segmented([["1", "24 hours"], ["7", "7 days"]], String(state.rangeDays), (v) => ((state.rangeDays = Number(v) as 1 | 7), renderChart()), "Range"),
  );
  if (!snap || snap.accounts.length === 0) {
    chartEmpty.hidden = false;
    replace(chartLegend);
    return;
  }
  const sinceMs = Date.now() - state.rangeDays * 86_400_000;
  const samples = await api.series(sinceMs);
  chartEmpty.hidden = samples.length > 0;
  replace(
    chartLegend,
    snap.accounts.map((a) => h("span", { class: "key" }, h("i", { style: { background: colorOf(a.id) } }), a.label)),
    h("span", { class: "key swap-key" }, h("i"), "Swap"),
  );
  chart.render({
    accounts: snap.accounts,
    samples,
    swaps: state.events.filter((e) => e.kind === "swap" && e.account_from !== null && e.ts >= sinceMs),
    color: colorOf,
    metric: state.metric,
    sinceMs,
    threshold: state.metric === "weekly" ? snap.settings.swap_threshold_7d : snap.settings.swap_threshold_5h,
  });
}

// ---- activity rail ---------------------------------------------------------------------

const REASONS: Record<string, string> = {
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

function describe(e: EventRow): { parts: Child[]; tone?: "warn" | "danger" } {
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

function renderActivity() {
  const routine = (e: EventRow) => ROUTINE.has(e.kind) || (e.kind === "state" && e.reason === "order");
  const items = state.events.filter((e) => state.showRoutine || !routine(e));
  if (items.length === 0) {
    replace(activity, h("li", { class: "muted empty" }, "Swaps, sign-ins and problems show up here."));
    return;
  }
  const rows: HTMLElement[] = [];
  let lastDay = "";
  for (const e of items.slice(0, 120)) {
    const day = dayName(e.ts);
    if (day !== lastDay) {
      rows.push(h("li", { class: "day" }, day));
      lastDay = day;
    }
    const d = describe(e);
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
  replace(activity, rows);
}

// ---- dialogs ---------------------------------------------------------------------------

function dialog(title: string, ...body: HTMLElement[]): HTMLDialogElement {
  const d = h("dialog", { class: "modal", "aria-label": title }, h("h2", null, title), ...body);
  d.addEventListener("close", () => d.remove());
  document.body.append(d);
  d.showModal();
  return d;
}

let currentLogin: { id: string; dialog: HTMLDialogElement; update: (p: LoginProgress) => void } | null = null;

function openLogin(account: AccountView | null) {
  const email = h("input", { type: "email", placeholder: "you@example.com", value: account?.email ?? "", autocomplete: "off" });
  const label = h("input", { type: "text", placeholder: "From the email if left empty", maxlength: 40, autocomplete: "off" });
  const steps = h("ol", { class: "steps" });
  const status = h("p", { class: "login-status muted" });
  const codeRow = h("div", { class: "code-row", hidden: true });
  const start = h("button", { class: "btn primary" }, "Open sign-in page");
  const cancel = h("button", { class: "btn quiet" }, "Cancel");

  let expectEmail: string | null = null;
  const found = h("div", { class: "found", hidden: true });
  const fields = h(
    "div",
    { class: "fields" },
    h("label", null, h("span", null, "Email"), email, h("small", { class: "muted" }, "Optional. Pre-fills the sign-in page.")),
    h("label", null, h("span", null, "Label"), label, h("small", { class: "muted" }, "Optional. A short name, like main or account2.")),
    h("p", { class: "muted" }, "Sign-in uses the claude.ai account your browser is signed in to. Switch accounts there first if you need to."),
  );
  const form = account
    ? h("p", null, `Sign in as ${account.email ?? account.label} in the browser window that opens. If the browser is signed in to another claude.ai account, switch accounts there first.`)
    : h("div", null, found, fields);

  const d = dialog(account ? `Sign in to ${account.label}` : "Add account", form, steps, status, codeRow, h("div", { class: "dialog-actions" }, cancel, start));

  // Accounts the other Claude Code profiles on this computer belong to: one click starts a fresh
  // sign-in with the email and the profile's name filled in. Those profiles are left untouched.
  if (!account) {
    api
      .otherProfiles()
      .then((profiles) => {
        if (profiles.length === 0) return;
        const taken = new Set(state.snap?.accounts.map((a) => a.label.toLowerCase()) ?? []);
        replace(
          found,
          h("h3", null, "From your other Claude Code profiles"),
          h(
            "ul",
            { class: "found-list" },
            profiles.map((p) =>
              h(
                "li",
                null,
                h(
                  "div",
                  { class: "found-who" },
                  h("div", { class: "who-label" }, p.email ?? p.name),
                  h("div", { class: "muted who-email" }, `${p.name} profile${p.subscription_type ? `, ${p.subscription_type[0].toUpperCase()}${p.subscription_type.slice(1)}` : ""}`),
                ),
                p.in_pool_as
                  ? h("span", { class: "muted found-state" }, `Added as ${p.in_pool_as}`)
                  : h(
                      "button",
                      {
                        class: "btn small",
                        onclick: () => {
                          email.value = p.email ?? "";
                          label.value = taken.has(p.name.toLowerCase()) ? "" : p.name;
                          expectEmail = p.email;
                          found.hidden = true;
                          fields.hidden = true;
                          d.querySelector("h2")!.textContent = `Add ${p.email ?? p.name}`;
                          start.click();
                        },
                      },
                      "Sign in",
                    ),
              ),
            ),
          ),
          h("h3", null, "Or add another account"),
        );
        found.hidden = false;
      })
      .catch(() => {});
  }

  const setSteps = (active: number) => {
    const labels = ["Open the sign-in page", "Sign in and approve in the browser", "Save the login"];
    replace(steps, labels.map((text, i) => h("li", { class: i < active ? "done" : i === active ? "now" : "" }, text)));
  };

  const update = (p: LoginProgress) => {
    switch (p.stage) {
      case "starting":
        setSteps(0);
        status.textContent = "Starting sign-in…";
        break;
      case "browser": {
        setSteps(1);
        status.replaceChildren(
          "Finish in your browser. ",
          h("button", { class: "link", onclick: () => p.url && api.openUrl(p.url) }, "Open the page again"),
        );
        const code = h("input", { type: "text", placeholder: "Code", autocomplete: "off" });
        replace(
          codeRow,
          h("label", null, h("span", null, "If the browser shows a code, paste it here"), code),
          h("button", { class: "btn small", onclick: () => code.value.trim() && api.submitLoginCode(p.id, code.value).catch((e) => say(String(e), "error")) }, "Submit"),
        );
        codeRow.hidden = false;
        break;
      }
      case "importing":
        setSteps(2);
        codeRow.hidden = true;
        status.textContent = "Saving login…";
        break;
      case "done":
        setSteps(3);
        currentLogin = null;
        d.close();
        say(p.account ? `${p.account.label} is ready${p.activated ? " and in use" : ""}` : "Login saved");
        refresh();
        break;
      case "failed":
        currentLogin = null;
        codeRow.hidden = true;
        status.className = "login-status error";
        status.textContent = p.message ?? "Sign-in didn't finish.";
        start.disabled = false;
        start.textContent = "Try again";
        cancel.textContent = "Close";
        break;
      case "cancelled":
        currentLogin = null;
        d.close();
        break;
    }
  };

  start.addEventListener("click", async () => {
    start.disabled = true;
    status.className = "login-status muted";
    form.querySelectorAll("input").forEach((i) => (i.disabled = true));
    try {
      const id = await api.startLogin(email.value.trim() || null, label.value.trim() || null, account?.id ?? null, expectEmail);
      currentLogin = { id, dialog: d, update };
    } catch (e) {
      start.disabled = false;
      form.querySelectorAll("input").forEach((i) => (i.disabled = false));
      status.className = "login-status error";
      status.textContent = String(e);
    }
  });
  cancel.addEventListener("click", () => {
    if (currentLogin) api.cancelLogin(currentLogin.id);
    else d.close();
  });
  d.addEventListener("cancel", (e) => {
    if (currentLogin) {
      e.preventDefault();
      api.cancelLogin(currentLogin.id);
    }
  });
}

function openManage(a: AccountView) {
  const label = h("input", { type: "text", value: a.label, maxlength: 40 });
  const error = h("p", { class: "login-status error", hidden: true });
  const save = h("button", { class: "btn primary" }, "Save");
  const remove = h("button", { class: "btn danger-outline", disabled: a.state === "active", title: a.state === "active" ? "Swap to another account first" : null }, "Remove account");
  const close = h("button", { class: "btn quiet" }, "Cancel");
  const run = (work: () => Promise<unknown>, message: string) => () => {
    d.close();
    act(work, () => message);
  };
  const accounts = state.snap?.accounts ?? [];
  const at = accounts.findIndex((x) => x.id === a.id);
  const quick = h(
    "div",
    { class: "quick" },
    a.state === "parked" && h("button", { class: "btn small", onclick: run(() => api.swapNow(a.id), `Swapped to ${a.label}`) }, "Use now"),
    a.state === "parked" && h("button", { class: "btn small", onclick: run(() => api.setPaused(a.id, true), `Paused ${a.label}`) }, "Pause"),
    a.state === "paused" && h("button", { class: "btn small", onclick: run(() => api.setPaused(a.id, false), `Resumed ${a.label}`) }, "Resume"),
    (a.state === "parked" || a.state === "paused") &&
      h("button", { class: "btn small", onclick: () => (d.close(), act(() => api.checkUsage(a.id), (r) => checkMessage(a.label, r))) }, "Check usage"),
    h("button", { class: "btn small", onclick: () => (d.close(), openLogin(a)) }, "Sign in again"),
  );
  const order = h(
    "div",
    { class: "order-row" },
    h("span", null, `${ordinal(at + 1)} of ${accounts.length} in your order`),
    h(
      "div",
      { class: "order-buttons" },
      h("button", { class: "btn small", disabled: at <= 0, onclick: () => (d.close(), moveBy(a, -1)) }, "Move up"),
      h("button", { class: "btn small", disabled: at >= accounts.length - 1, onclick: () => (d.close(), moveBy(a, 1)) }, "Move down"),
    ),
  );
  const weekly = h(
    "select",
    {
      onchange: (e: Event) => {
        const value = (e.target as HTMLSelectElement).value as WeeklyLimitSetting;
        api
          .setWeeklyLimit(a.id, value)
          .then(() => (say("Weekly limit saved"), refresh()))
          .catch((err) => say(String(err), "error"));
      },
    },
    (
      [
        ["auto", `Detect from usage (${WEEKLY_TEXT[a.weekly.detected]})`],
        ["yes", "Has a weekly limit"],
        ["no", "No weekly limit"],
      ] as const
    ).map(([value, text]) => h("option", { value, selected: a.weekly.setting === value }, text)),
  );
  const weeklyThreshold = state.snap?.settings.swap_threshold_7d ?? 98;
  const cap = h("input", {
    type: "number",
    min: 1,
    max: 100,
    step: "any",
    value: a.weekly_cap === null ? "" : String(a.weekly_cap),
    placeholder: String(weeklyThreshold),
  });
  // Filled in once the history is read, which takes a moment.
  const history = h("dd", { class: "recent-weeks" }, h("span", { class: "muted" }, "Loading…"));
  const value = h("dd", { title: "What this use would have cost at API prices, as Claude Code estimates it" }, h("span", { class: "muted" }, "Loading…"));
  api
    .usageHistory(a.id)
    .then((hist) => {
      replace(history, recentWeeks(hist));
      replace(value, valueText(hist));
    })
    .catch(() => {
      replace(history, h("span", { class: "muted" }, "couldn't be read"));
      replace(value, h("span", { class: "muted" }, "couldn't be read"));
    });
  const useExpiring = h("input", { type: "checkbox", checked: a.use_expiring });
  const keepRunning = h("input", { type: "checkbox", checked: a.keep_running });
  const checksOff = state.snap?.settings.usage_checks === false;
  const details = h(
    "dl",
    { class: "facts info" },
    h("dt", null, "Email"),
    h("dd", null, a.email ?? "unknown"),
    a.org_name && h("dt", null, "Organization"),
    a.org_name && h("dd", null, a.org_name),
    a.plan && h("dt", null, "Plan"),
    a.plan && h("dd", null, a.plan),
    h("dt", null, "Recent weeks"),
    history,
    h("dt", null, "API value"),
    value,
    a.extra_usage && h("dt", null, "Extra usage"),
    a.extra_usage &&
      h(
        "dd",
        { class: a.extra_usage.enabled ? "warn" : null, title: `As of ${new Date(a.extra_usage.read_at).toLocaleString()}` },
        extraText(a.extra_usage),
      ),
    h("dt", null, "Pick order"),
    h("dd", null, a.rank ? `${ordinal(a.rank)} - ${a.reason}` : `not available${a.reason ? ` - ${a.reason}` : ""}`),
    h("dt", null, "Login updated"),
    h("dd", null, a.login_updated_at ? ago(a.login_updated_at) : "never"),
    a.refresh_expires_at && a.state !== "needs_login" && h("dt", null, "Sign-in ends"),
    a.refresh_expires_at &&
      a.state !== "needs_login" &&
      h(
        "dd",
        { class: a.sign_in_ends_soon ? "warn" : null, title: new Date(a.refresh_expires_at).toLocaleString() },
        a.refresh_expires_at > Date.now() ? `in ${until(a.refresh_expires_at / 1000)}` : "ended",
      ),
    h("dt", null, "Usage check"),
    h("dd", null, checkText(a)),
    a.keep_running && h("dt", null, "Keeping it running"),
    a.keep_running && h("dd", null, warmText(a)),
  );
  const d = dialog(
    a.label,
    details,
    quick,
    order,
    h(
      "div",
      { class: "fields" },
      h("label", null, h("span", null, "Weekly limit"), weekly, h("small", { class: "muted" }, "Detected from the usage Claude Code reports.")),
      a.weekly.effective !== "none" &&
        h(
          "label",
          null,
          h("span", null, "Stop using it at weekly usage (%)"),
          cap,
          h("small", { class: "muted" }, `Keeps the rest of its week for you. Empty uses ${weeklyThreshold}% from Settings.`),
        ),
      a.weekly.effective !== "none" &&
        h("label", { class: "check" }, useExpiring, "Use its weekly budget before it resets"),
      h(
        "div",
        { class: "check-field" },
        h("label", { class: "check" }, keepRunning, "Keep its 5-hour window running"),
        h(
          "small",
          { class: "muted" },
          "While it's not in use, starts its next window with a short Haiku request as soon as one ends, so a window is already counting down when it's needed.",
          checksOff ? " Needs usage checks, which are off in Settings." : "",
        ),
      ),
      h("label", null, h("span", null, "Label"), label),
    ),
    error,
    h("div", { class: "dialog-actions spread" }, remove, h("div", null, close, save)),
  );
  close.addEventListener("click", () => d.close());
  save.addEventListener("click", async () => {
    try {
      // Only a change is checked and saved: a cap set from the CLI can be any number from 1 to 100.
      const capValue = cap.value.trim() === "" ? null : Number(cap.value);
      if (cap.validity.badInput || (capValue !== null && !(Number.isFinite(capValue) && capValue >= 1 && capValue <= 100))) {
        throw new Error("Use a number from 1 to 100, or leave it empty.");
      }
      // Each saved change becomes the new baseline, so a retry after a failed rename does not
      // save (and log) the others again.
      if (capValue !== a.weekly_cap) {
        await api.setWeeklyCap(a.id, capValue);
        a.weekly_cap = capValue;
      }
      if (useExpiring.checked !== a.use_expiring) {
        await api.setUseExpiring(a.id, useExpiring.checked);
        a.use_expiring = useExpiring.checked;
      }
      if (keepRunning.checked !== a.keep_running) {
        await api.setKeepRunning(a.id, keepRunning.checked);
        a.keep_running = keepRunning.checked;
      }
      if (label.value.trim() !== a.label) await api.renameAccount(a.id, label.value.trim());
      d.close();
      await refresh();
    } catch (e) {
      error.hidden = false;
      error.textContent = String(e);
    }
  });
  let armed = false;
  remove.addEventListener("click", async () => {
    if (!armed) {
      armed = true;
      remove.textContent = `Yes, remove ${a.label}`;
      return;
    }
    try {
      await api.removeAccount(a.id);
      d.close();
      say(`Removed ${a.label}`);
      await refresh();
    } catch (e) {
      error.hidden = false;
      error.textContent = String(e);
    }
  });
}

async function openSettings() {
  const checks = h("ul", { class: "checks" }, h("li", { class: "muted" }, "Running checks…"));
  const info = h("dl", { class: "facts info" });
  const settingsForm = h("div", { class: "fields" });
  const autostart = h("input", { type: "checkbox", id: "autostart" });
  const d = dialog(
    "Settings",
    h("h3", null, "Health"),
    checks,
    h("h3", null, "Rotation"),
    settingsForm,
    h("h3", null, "General"),
    h("label", { class: "check", for: "autostart" }, autostart, "Start cc-rotate when I sign in"),
    h("h3", null, "About"),
    info,
    h("div", { class: "dialog-actions" }, h("button", { class: "btn", onclick: () => d.close() }, "Done")),
  );
  d.classList.add("drawer");

  const snap = state.snap;
  if (snap) {
    const field = (key: string, text: string, value: number, hint: string, min: number, max: number) => {
      const input = h("input", { type: "number", value: String(value), min, max, step: 1 });
      input.addEventListener("change", () =>
        api.setSetting(key, input.value).then(() => (say("Saved"), refresh())).catch((e) => say(String(e), "error")),
      );
      return h("label", null, h("span", null, text), input, h("small", { class: "muted" }, hint));
    };
    const checks = h("input", { type: "checkbox", id: "usage-checks", checked: snap.settings.usage_checks });
    checks.addEventListener("change", () =>
      api
        .setSetting("usage_checks", String(checks.checked))
        .then(() => (say(checks.checked ? "Usage checks are on" : "Usage checks are off"), refresh()))
        .catch((e) => say(String(e), "error")),
    );
    const prefer = h("input", { type: "checkbox", id: "prefer-no-weekly", checked: snap.settings.prefer_no_weekly_limit });
    const expiringBox = h("input", { type: "checkbox", id: "use-expiring", checked: snap.settings.use_expiring_weekly });
    const strictBox = h("input", { type: "checkbox", id: "strict-order", checked: snap.settings.strict_order });
    strictBox.addEventListener("change", () =>
      api
        .setSetting("strict_order", String(strictBox.checked))
        .then(() => (say(strictBox.checked ? "Accounts higher in your order are used again as soon as they're ready" : "The account in use is kept until a limit"), refresh()))
        .catch((e) => say(String(e), "error")),
    );
    const avoidPaid = h("input", { type: "checkbox", id: "avoid-extra-usage", checked: snap.settings.avoid_extra_usage });
    avoidPaid.addEventListener("change", () =>
      api
        .setSetting("avoid_extra_usage", String(avoidPaid.checked))
        .then(() =>
          (say(avoidPaid.checked ? "Sessions will wait instead of using paid extra usage" : "Accounts with extra usage keep working when every account is at a limit"), refresh()),
        )
        .catch((e) => say(String(e), "error")),
    );
    expiringBox.addEventListener("change", () =>
      api
        .setSetting("use_expiring_weekly", String(expiringBox.checked))
        .then(() => (say(expiringBox.checked ? "Weekly budget is used before it resets" : "Weekly budget is no longer moved ahead"), refresh()))
        .catch((e) => say(String(e), "error")),
    );
    prefer.addEventListener("change", () =>
      api
        .setSetting("prefer_no_weekly_limit", String(prefer.checked))
        .then(() => (say(prefer.checked ? "Accounts without a weekly limit are used first" : "Accounts with and without a weekly limit are used alike"), refresh()))
        .catch((e) => say(String(e), "error")),
    );
    replace(
      settingsForm,
      field("swap_threshold_5h", "Swap at 5-hour usage (%)", snap.settings.swap_threshold_5h, "Swap before a 5-hour limit is reached.", 1, 100),
      field("swap_threshold_7d", "Swap at weekly usage (%)", snap.settings.swap_threshold_7d, "Also applies to weekly limits on a single model.", 1, 100),
      field("swap_cooldown_s", "Minimum time between swaps (seconds)", snap.settings.swap_cooldown_s, "Applies to swaps at a limit and swaps back.", 0, 3600),
      field(
        "switch_back_dwell_s",
        "Swap back after (seconds)",
        snap.settings.switch_back_dwell_s,
        "Minimum time on an account before swapping back to a better one.",
        0,
        86400,
      ),
      h("label", { class: "check", for: "prefer-no-weekly" }, prefer, "Use accounts without a weekly limit first"),
      h("label", { class: "check", for: "strict-order" }, strictBox, "Always go back to accounts higher in my order"),
      h("label", { class: "check", for: "use-expiring" }, expiringBox, "Use weekly budget before it resets"),
      h(
        "label",
        { class: "check", for: "avoid-extra-usage" },
        avoidPaid,
        "When every account is at a limit, wait instead of paying for extra usage",
      ),
      h("label", { class: "check", for: "usage-checks" }, checks, "Check usage of accounts not in use"),
      field(
        "usage_check_interval_s",
        "Check every (seconds)",
        snap.settings.usage_check_interval_s,
        "Runs Claude Code's /usage, which doesn't count against limits. Minimum 300.",
        300,
        86400,
      ),
      field(
        "usage_check_max_age_s",
        "Check before swapping if usage is older than (seconds)",
        snap.settings.usage_check_max_age_s,
        "Set to 0 to always check first.",
        0,
        86400,
      ),
      field(
        "login_warning_days",
        "Warn before a sign-in ends (days)",
        snap.settings.login_warning_days,
        "Each sign-in lasts about a month. Set to 0 to turn the warning off.",
        0,
        30,
      ),
    );
  }
  api.autostartEnabled().then((on) => (autostart.checked = on)).catch(() => (autostart.disabled = true));
  autostart.addEventListener("change", () =>
    api.setAutostart(autostart.checked).then(() => say(autostart.checked ? "cc-rotate will start when you sign in" : "cc-rotate won't start when you sign in")).catch((e) => say(String(e), "error")),
  );

  const [results, about] = await Promise.all([api.doctor().catch(() => []), api.appInfo().catch(() => null)]);
  replace(
    checks,
    results.map((c) => h("li", { class: c.status }, h("strong", null, c.name), h("span", null, c.detail))),
  );
  if (about) {
    replace(
      info,
      h("dt", null, "cc-rotate"),
      h("dd", null, about.version),
      h("dt", null, "Claude Code"),
      h("dd", null, about.claude_version ?? "not found"),
      h("dt", null, "Profile folder"),
      h("dd", { class: "path" }, about.profile_dir),
      h("dt", null, "Data folder"),
      h("dd", { class: "path" }, about.data_dir),
    );
  }
}

// ---- data flow -------------------------------------------------------------------------

let refreshing: Promise<void> | null = null;
async function refresh() {
  if (refreshing) return refreshing;
  refreshing = (async () => {
    try {
      const [snap, events] = await Promise.all([api.snapshot(), api.events(300)]);
      // An order the user just changed is still being saved; this read may predate it.
      if (pendingOrderSaves > 0) return;
      state.snap = snap;
      state.events = events;
      colorOf = colorFor(snap.accounts);
      renderTop();
      renderOverview();
      renderAccounts();
      renderActivity();
      await renderChart();
    } catch (e) {
      say(`Couldn't load cc-rotate's data: ${e}`, "error");
    } finally {
      refreshing = null;
    }
  })();
  return refreshing;
}

onStoreChanged(() => refresh());
onLoginProgress((p) => {
  if (currentLogin && p.id === currentLogin.id) currentLogin.update(p);
});
window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => renderChart());
// Countdowns and effective usage move with the clock even when nothing is written.
window.setInterval(() => refresh(), 30_000);
refresh();
