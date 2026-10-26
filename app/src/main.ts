import "@fontsource-variable/schibsted-grotesk";
import "./styles.css";

import { activityItems, routine } from "./activity";
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
  type Settings,
  type Snapshot,
  type WeeklyLimitSetting,
  type WindowUsage,
} from "./api";
import { colorFor, usd } from "./chart";
import { h, replace, type Child } from "./dom";
import { ago, clock, ordinal, pct, until } from "./format";
import { ChartPanel } from "./panel";
import { dragRow, keyMove } from "./sortable";
import { axis, fitLabels, laneEl, lanes, nextUp, overlay, planView, stateLine, summary } from "./timeline";

type Page = "overview" | "activity" | "cost" | "settings";
type Route = { page: Page } | { page: "account"; id: number };

interface UiState {
  snap: Snapshot | null;
  events: EventRow[];
  route: Route;
  showRoutine: boolean;
  busy: boolean;
  /** A row is being dragged; renders wait until it is dropped. */
  dragging: boolean;
  /** The `data-focus` key of the element that should have focus after the next render. */
  focus: string | null;
}

const state: UiState = {
  snap: null,
  events: [],
  route: { page: "overview" },
  showRoutine: false,
  busy: false,
  dragging: false,
  focus: null,
};

// ---- layout ----------------------------------------------------------------------------

const root = document.getElementById("app")!;
const nav = h("nav", { class: "side", "aria-label": "cc-rotate" });
const head = h("header", { class: "page-head" });
const banner = h("div", { class: "banner", hidden: true });
const body = h("div", { class: "page-body" });
const toast = h("div", { class: "toast", role: "status", "aria-live": "polite" });
root.append(nav, h("main", { class: "page" }, head, banner, body), toast);

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
/** No weekly limit: the 5-hour window is what runs out. */
const noWeekly = (a: AccountView) => a.weekly.effective === "none";

/** Replace `el`'s children, keeping keyboard focus on the element with the same `data-focus` key. */
function rerender(el: HTMLElement, ...children: Child[]) {
  const active = document.activeElement as HTMLElement | null;
  const had = active && el.contains(active) ? active.dataset.focus : undefined;
  replace(el, ...children);
  const want = state.focus ?? had;
  if (!want) return;
  const target = el.querySelector<HTMLElement>(`[data-focus="${CSS.escape(want)}"]`);
  if (!target) return;
  state.focus = null;
  // A button that turned off (Move up at the top) hands focus to its alternative.
  const alt = target.matches(":disabled") && target.dataset.focusAlt ? el.querySelector<HTMLElement>(`[data-focus="${CSS.escape(target.dataset.focusAlt)}"]`) : null;
  (alt ?? target).focus();
}

const SVG_NS = "http://www.w3.org/2000/svg";

/** A 16px stroke icon from path data. */
function icon(...paths: string[]): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  for (const [k, v] of Object.entries({ viewBox: "0 0 16 16", width: "16", height: "16", fill: "none", stroke: "currentColor", "stroke-width": "1.6", "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" })) {
    svg.setAttribute(k, v);
  }
  for (const d of paths) {
    const p = document.createElementNS(SVG_NS, "path");
    p.setAttribute("d", d);
    svg.append(p);
  }
  return svg;
}

const ICONS: Record<Page, () => SVGSVGElement> = {
  overview: () => icon("M2 4h12", "M2 8h7", "M2 12h10", "M11.5 6.5v3"),
  activity: () => icon("M2 8h3l2-5 2 10 2-5h3"),
  cost: () => icon("M3 13V9", "M8 13V3", "M13 13V6"),
  settings: () => icon("M8 5.8a2.2 2.2 0 1 0 0 4.4 2.2 2.2 0 0 0 0-4.4Z", "M8 1.8v2", "M8 12.2v2", "M1.8 8h2", "M12.2 8h2", "M3.6 3.6 5 5", "M11 11l1.4 1.4", "M3.6 12.4 5 11", "M11 5l1.4-1.4"),
};

const GRIP = () => {
  const svg = document.createElementNS(SVG_NS, "svg");
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
    const dot = document.createElementNS(SVG_NS, "circle");
    dot.setAttribute("cx", String(cx));
    dot.setAttribute("cy", String(cy));
    dot.setAttribute("r", "1.4");
    svg.append(dot);
  }
  return svg;
};

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

/** The usage check facts on the account page. */
function checkText(a: AccountView): string {
  const c = a.usage_check;
  if (a.state === "active") return "not needed while in use";
  if (c.held_back) return "on hold until a check confirms its login";
  if (c.problem) return c.problem;
  const read = c.last_read_at ? `checked ${ago(c.last_read_at)}` : "not checked yet";
  return c.not_before && c.not_before > Date.now() ? `${read}, next after ${clock(c.not_before)}` : read;
}

/** A warm-up's problem is shown this long after its last try (`WARM_PROBLEM_SHOWN_MS`). */
const WARM_PROBLEM_SHOWN_MS = 6 * 3_600_000;

function warmText(a: AccountView): string {
  const c = a.usage_check;
  const recent = c.warm_attempt_at !== null && Date.now() - c.warm_attempt_at < WARM_PROBLEM_SHOWN_MS;
  if (c.warm_problem && recent) return `last try failed: ${c.warm_problem}`;
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

/** "API value" on the account page: what the last weeks' use would have cost at API prices. */
function valueText(hist: AccountHistory): string {
  const counted = hist.weeks.filter((w) => w.api_value_usd !== null);
  if (counted.length === 0) return "nothing counted yet";
  const total = counted.reduce((sum, w) => sum + (w.api_value_usd ?? 0), 0);
  const weeks = hist.weeks.length;
  return `${dollars(total)} in the last ${weeks === 1 ? "week" : `${weeks} weeks`}`;
}

/** Weeks "Recent weeks" shows on the account page. */
const RECENT_WEEKS = 4;

/**
 * "Recent weeks" on the account page, newest first: the highest usage of each weekly window
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

/** When the account in use was swapped in, from the activity log. */
function activeSince(): number | null {
  const id = state.snap?.active_id;
  if (id === null || id === undefined) return null;
  return state.events.find((e) => (e.kind === "swap" || e.kind === "adopted_login") && e.account_to === id)?.ts ?? null;
}

const swapNow = () =>
  act(
    () => api.swapNow(null),
    (r) => (r.outcome === "swapped" ? `Swapped to ${labelOf(r.to)}` : r.outcome === "pool_exhausted" ? "No other account is ready" : `No swap: ${r.why}`),
  );
const useNow = (a: AccountView) => act(() => api.swapNow(a.id), () => `Swapped to ${a.label}`);
const resume = (a: AccountView) => act(() => api.setPaused(a.id, false), () => `Resumed ${a.label}`);

// ---- navigation ------------------------------------------------------------------------

const isPage = (page: Route["page"]) => state.route.page === page;
const isAccount = (id: number) => state.route.page === "account" && state.route.id === id;

/** Pages that hold a form are built when opened, so a refresh never undoes what's being typed. */
let settingsPage: HTMLElement | null = null;
let accountForm: { id: number; el: HTMLElement } | null = null;
let accountHistory: { id: number; hist: AccountHistory | null; failed: boolean } | null = null;

function go(route: Route) {
  const same = route.page === state.route.page && (route.page !== "account" || (state.route.page === "account" && state.route.id === route.id));
  if (same) {
    body.scrollTop = 0;
    return;
  }
  state.route = route;
  settingsPage = null;
  accountForm = null;
  if (route.page === "account") {
    const id = route.id;
    accountHistory = { id, hist: null, failed: false };
    api
      .usageHistory(id)
      .then((hist) => accountHistory?.id === id && ((accountHistory.hist = hist), render()))
      .catch(() => accountHistory?.id === id && ((accountHistory.failed = true), render()));
  }
  body.scrollTop = 0;
  render();
  if (route.page === "cost") {
    costChart.refresh();
    loadCostTable();
  }
  if (route.page === "overview") overviewChart.refresh();
}

// ---- order -----------------------------------------------------------------------------

/** Order saves run one after another, so quick key presses are never dropped. */
let orderSaves = Promise.resolve();
let pendingOrderSaves = 0;
/** Bumped by every order change, so a read that started before one is not shown. */
let orderVersion = 0;

/** Save a new order: optimistic in the UI, then written to the store. */
function saveOrder(ids: number[], moved: AccountView) {
  const snap = state.snap;
  if (!snap) return;
  const before = snap.accounts.map((a) => a.id);
  if (ids.every((id, i) => id === before[i])) return;
  snap.accounts.sort((x, y) => ids.indexOf(x.id) - ids.indexOf(y.id));
  orderVersion += 1;
  render();
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
  saveOrder(ids, a);
}

function startDrag(e: PointerEvent, a: AccountView) {
  if (state.busy) return;
  const grip = e.currentTarget as HTMLElement;
  const row = grip.closest<HTMLElement>("[data-id]");
  if (!row?.parentElement) return;
  const rows = [...row.parentElement.children].filter((el): el is HTMLElement => el instanceof HTMLElement && el.dataset.id !== undefined);
  dragRow(e, grip, {
    rows,
    row,
    started: () => (state.dragging = true),
    dropped: (to) => {
      state.dragging = false;
      const ids = rows.map((r) => Number(r.dataset.id)).filter((id) => id !== a.id);
      ids.splice(to, 0, a.id);
      saveOrder(ids, a);
    },
    ended: () => {
      if (!state.dragging) return;
      state.dragging = false;
      render();
    },
  });
}

/** An account's drag handle, in the sidebar or the plan. */
function grip(a: AccountView, at: number, count: number, where: "side" | "plan") {
  return h(
    "button",
    {
      class: "grip",
      type: "button",
      title: "Drag to reorder",
      "aria-label": `${a.label}, ${ordinal(at + 1)} of ${count} in your order. Use the up and down arrow keys to move it.`,
      "data-focus": `grip-${where}-${a.id}`,
      onpointerdown: (e: PointerEvent) => startDrag(e, a),
      onkeydown: (e: KeyboardEvent) => {
        if (!["ArrowUp", "ArrowDown", "Home", "End"].includes(e.key)) return;
        e.preventDefault();
        const delta = keyMove(e, at, count);
        if (delta === 0) return;
        state.focus = `grip-${where}-${a.id}`;
        moveBy(a, delta);
      },
    },
    GRIP(),
  );
}

/** How cc-rotate uses the order, as the settings make it. */
function orderNote(s: Settings): string {
  const first =
    s.prefer_no_weekly_limit && s.use_expiring_weekly
      ? "Accounts without a weekly limit go first, then weekly budget that would go unused."
      : s.prefer_no_weekly_limit
        ? "Accounts without a weekly limit go first."
        : s.use_expiring_weekly
          ? "Weekly budget that would go unused goes first."
          : "";
  const within = s.strict_order
    ? "top to bottom"
    : "the account in use stays until a limit, then one whose 5-hour window is already running comes first, then the rest top to bottom";
  const note = first ? `${first} Within each group, ${within}.` : `${within[0].toUpperCase()}${within.slice(1)}.`;
  return s.prefer_no_weekly_limit ? `${note} An account with a weekly limit gives way as soon as one without is ready.` : note;
}

// ---- sidebar ---------------------------------------------------------------------------

function sideState(a: AccountView): { text: string; tone: string } {
  switch (a.state) {
    case "active":
      return { text: "In use", tone: "on" };
    case "paused":
      return { text: "Paused", tone: "" };
    case "needs_login":
      return { text: "Sign in", tone: "danger" };
    case "parked":
      if (a.eligible) return { text: state.snap && nextUp(state.snap) === a.id ? "Next" : "Ready", tone: "" };
      if (!a.available_at) return { text: "Waiting", tone: "warn" };
      // Today's time, or the weekday for a weekly limit days away.
      return { text: a.available_at * 1000 - Date.now() < 20 * 3_600_000 ? new Date(a.available_at * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }) : new Date(a.available_at * 1000).toLocaleDateString([], { weekday: "short" }), tone: "warn" };
  }
}

function renderNav() {
  const snap = state.snap;
  const accounts = snap?.accounts ?? [];
  const item = (page: Page, label: string, count?: string | null) =>
    h(
      "button",
      { class: "nav-item", "data-focus": `nav-${page}`, "aria-current": isPage(page) ? "page" : null, onclick: () => go({ page }) },
      ICONS[page](),
      h("span", null, label),
      count && h("span", { class: "count" }, count),
    );
  rerender(
    nav,
    h("div", { class: "brand" }, h("img", { src: "/icon.svg", alt: "", width: 22, height: 22 }), "cc-rotate"),
    h(
      "div",
      { class: "nav-items" },
      item("overview", "Overview"),
      item("activity", "Activity", snap ? `${snap.pool.swaps_24h} ${snap.pool.swaps_24h === 1 ? "swap" : "swaps"}` : null),
      item("cost", "API cost"),
      item("settings", "Settings"),
    ),
    h(
      "div",
      { class: "side-group" },
      h("h2", null, "Accounts"),
      h("button", { class: "icon-btn", title: "Add account", "aria-label": "Add account", "data-focus": "nav-add", onclick: () => openLogin(null) }, icon("M8 3v10", "M3 8h10")),
    ),
    accounts.length === 0
      ? h("p", { class: "side-note" }, "No accounts yet.")
      : h(
          "ol",
          { class: "side-accounts", "aria-label": "Accounts, in your order" },
          accounts.map((a, i) => {
            const five = fiveOf(a);
            const left = a.state === "needs_login" ? 0 : five ? Math.max(0, 100 - five.pct) : 100;
            const st = sideState(a);
            return h(
              "li",
              { class: "side-acct", "data-id": a.id },
              grip(a, i, accounts.length, "side"),
              h(
                "button",
                {
                  class: "side-link",
                  "data-focus": `acct-${a.id}`,
                  "aria-current": isAccount(a.id) ? "page" : null,
                  title: a.email ?? a.label,
                  onclick: () => go({ page: "account", id: a.id }),
                },
                h("span", { class: "swatch", style: { background: colorOf(a.id) } }),
                h("span", { class: "side-name" }, a.label),
                h("span", { class: `side-state ${st.tone}`.trim() }, st.text),
                h("span", { class: five ? "mini" : "mini unread", "aria-hidden": "true" }, h("i", { style: { width: `${left}%`, background: colorOf(a.id) } })),
              ),
            );
          }),
        ),
    accounts.length > 1 && h("p", { class: "side-note" }, "Drag to change your order. Bars show the 5-hour window left."),
  );
}

// ---- page head and banner --------------------------------------------------------------

function renderHead() {
  const snap = state.snap;
  const r = state.route;
  const title = (text: Child, sub?: Child) => [h("div", { class: "page-title" }, h("h1", null, text), sub && h("p", { class: "page-sub" }, sub))];
  const swap =
    snap && snap.accounts.length > 0
      ? h(
          "button",
          {
            class: "btn",
            disabled: !snap.pool.eligible,
            title: snap.pool.eligible ? "Swap to the next account in line" : "No other account is ready",
            onclick: swapNow,
          },
          "Swap now",
        )
      : null;
  const add = h("button", { class: "btn primary", onclick: () => openLogin(null) }, "Add account");
  if (r.page === "overview") {
    const active = snap?.accounts.find((a) => a.id === snap.active_id);
    const since = activeSince();
    rerender(head, title("Overview", active ? ["Using ", h("b", null, active.label), since ? ` since ${clock(since)}` : ""] : null), h("div", { class: "page-actions" }, swap, add));
  } else if (r.page === "account") {
    const a = snap?.accounts.find((x) => x.id === r.id);
    if (!a) return replace(head, title("Account"));
    const btn = (text: string, key: string, onclick: () => void, primary = false) => h("button", { class: primary ? "btn primary" : "btn", "data-focus": `head-${key}`, onclick }, text);
    rerender(
      head,
      h(
        "div",
        { class: "page-title" },
        h("h1", null, h("span", { class: "swatch big", style: { background: colorOf(a.id) } }), a.label, a.plan && h("span", { class: "plan" }, a.plan)),
        h("p", { class: "page-sub" }, h("span", { class: `badge ${a.state}${a.state === "parked" && !a.eligible ? " resting" : ""}` }, stateText(a)), " ", a.email ?? ""),
      ),
      h(
        "div",
        { class: "page-actions" },
        a.state === "parked" && btn("Use now", "use", () => useNow(a), true),
        a.state === "parked" && btn("Pause", "pause", () => act(() => api.setPaused(a.id, true), () => `Paused ${a.label}`)),
        a.state === "paused" && btn("Resume", "pause", () => resume(a), true),
        (a.state === "parked" || a.state === "paused") && btn("Check usage", "check", () => act(() => api.checkUsage(a.id), (res) => checkMessage(a.label, res))),
        btn("Sign in again", "sign-in", () => openLogin(a), a.state === "needs_login"),
      ),
    );
  } else if (r.page === "activity") {
    const toggle = h("input", {
      type: "checkbox",
      id: "routine",
      checked: state.showRoutine,
      "data-focus": "routine",
      onchange: (e: Event) => {
        state.showRoutine = (e.target as HTMLInputElement).checked;
        renderPage();
      },
    });
    rerender(head, title("Activity", snap ? `${snap.pool.swaps_24h} ${snap.pool.swaps_24h === 1 ? "swap" : "swaps"} in the last 24 hours` : null), h("label", { class: "check", for: "routine" }, toggle, "Show renewals and other routine events"));
  } else if (r.page === "cost") {
    rerender(head, title("API cost", "What your use would have cost at API prices, as Claude Code estimates it"));
  } else {
    rerender(head, title("Settings"));
  }
}

function renderBanner() {
  const snap = state.snap;
  const show = (tone: "warn" | "danger", ...children: Child[]) => {
    banner.hidden = false;
    banner.className = `banner ${tone}`;
    rerender(banner, ...children);
  };
  const needs = snap?.accounts.filter((a) => a.state === "needs_login") ?? [];
  if (snap?.pool.swap_unsettled) {
    show("warn", h("span", null, "A swap was interrupted. Swaps are on hold until cc-rotate confirms which account the profile is signed in to. Your sessions keep working."));
  } else if (needs.length > 0) {
    show(
      "danger",
      h(
        "span",
        null,
        needs.length === 1 ? `${needs[0].label} is signed out. Sign in again to use it.` : `${needs.map((a) => a.label).join(", ")} are signed out. Sign in again to use them.`,
      ),
      h("button", { class: "btn small", "data-focus": "banner-action", onclick: () => openLogin(needs[0]) }, `Sign in to ${needs[0].label}`),
    );
  } else if (snap && snap.pool.sessions_waiting > 0) {
    show("warn", h("span", null, `${snap.pool.sessions_waiting === 1 ? "A session is" : `${snap.pool.sessions_waiting} sessions are`} stopped at a limit, waiting for an account with room.`));
  } else if (snap?.pool.exhausted && snap.accounts.length > 0 && !isPage("overview")) {
    // The overview's headline says it there.
    const next = snap.pool.next_available_at;
    show("warn", h("span", null, "No account is ready to take over. ", next ? `The next one frees up in ${until(next)}.` : ""));
  } else if (snap && snap.pool.sign_ins_ending.length > 0) {
    // The sign-in that ends first, first.
    const ending = snap.accounts.filter((a) => a.sign_in_ends_soon).sort((x, y) => (x.refresh_expires_at ?? 0) - (y.refresh_expires_at ?? 0));
    const first = ending[0];
    show(
      "warn",
      h(
        "span",
        null,
        ending.length === 1 && first.refresh_expires_at
          ? `${first.label}'s sign-in ends in ${until(first.refresh_expires_at / 1000)}. Sign in again to keep it in rotation.`
          : `${ending.map((a) => a.label).join(", ")} need a new sign-in soon. Sign in again to keep them in rotation.`,
      ),
      h("button", { class: "btn small", "data-focus": "banner-action", onclick: () => openLogin(first) }, `Sign in to ${first.label}`),
    );
  } else {
    banner.hidden = true;
  }
}

// ---- overview --------------------------------------------------------------------------

const chartContext = {
  snap: () => state.snap,
  color: (id: number) => colorOf(id),
  labelOf,
  error: (message: string) => say(message, "error"),
};
const overviewChart = new ChartPanel(chartContext, ["usage", "cost"], "overview-chart");
const costChart = new ChartPanel(chartContext, ["cost"], "cost-chart");

function onboarding(): Child {
  return h(
    "section",
    { class: "onboard" },
    h("h2", null, "Add your first account"),
    h(
      "p",
      null,
      "Sign in to each of your Claude accounts once. cc-rotate keeps the logins encrypted on this computer and swaps the cc-rotate profile to another account when one nears its limits.",
    ),
    h("button", { class: "btn primary", onclick: () => openLogin(null) }, "Add account"),
  );
}

function planSection(snap: Snapshot): Child {
  const view = planView(Date.now());
  const all = lanes(snap, activeSince(), view, (id) => labelOf(id));
  const count = snap.accounts.length;
  const next = nextUp(snap);
  const rows = snap.accounts.map((a, i) => {
    const action =
      a.state === "needs_login"
        ? h("button", { class: "row-action danger", "data-focus": `row-${a.id}`, onclick: () => openLogin(a) }, "Sign in")
        : a.state === "paused"
          ? h("button", { class: "row-action", "data-focus": `row-${a.id}`, onclick: () => resume(a) }, "Resume")
          : a.state === "parked" && a.eligible
            ? h("button", { class: "row-action quiet", "data-focus": `row-${a.id}`, onclick: () => useNow(a), title: `Swap to ${a.label} now` }, "Use now")
            : null;
    const st = stateLine(a, next === a.id);
    return h(
      "li",
      { class: `plan-row${a.id === snap.active_id ? " is-active" : ""}`, "data-id": a.id, style: { "--c": colorOf(a.id) } },
      h(
        "div",
        { class: "plan-info" },
        grip(a, i, count, "plan"),
        h(
          "button",
          { class: "plan-who", "data-focus": `plan-${a.id}`, title: a.reason ? `${a.rank ? `${ordinal(a.rank)} pick: ` : ""}${a.reason}` : null, onclick: () => go({ page: "account", id: a.id }) },
          h("span", { class: "plan-name" }, h("span", { class: "swatch" }), a.label, a.plan && h("span", { class: "plan-tag" }, a.plan)),
          h("span", { class: `plan-state ${a.state === "needs_login" ? "danger" : ""}`.trim() }, st),
        ),
        action,
      ),
      laneEl(all.get(a.id)!, view, colorOf(a.id)),
    );
  });
  const p = snap.pool;
  return h(
    "section",
    { class: "card plan-card", "aria-label": "Plan for the next 12 hours" },
    h("div", { class: "plan-top" }, h("div", { class: "plan-corner" }, "Your order"), axis(snap, view)),
    h("div", { class: "plan-rows" }, overlay(snap, view), h("ol", { class: "plan-list" }, rows)),
    h(
      "div",
      { class: "plan-foot" },
      h(
        "div",
        { class: "plan-keys", "aria-hidden": "true" },
        h("span", null, h("i", { class: "k-use" }), "In use"),
        h("span", null, h("i", { class: "k-next" }), "Planned"),
        h("span", null, h("i", { class: "k-rest" }), "Waiting for a limit to reset"),
      ),
      h("p", { class: "muted" }, orderNote(snap.settings), p.weekly_limited > 0 ? ` Weekly budget left: ${p.weekly_accounts_left.toFixed(1)} of ${p.weekly_limited} ${p.weekly_limited === 1 ? "account" : "accounts"}.` : ""),
    ),
  );
}

function renderOverview() {
  const snap = state.snap;
  if (!snap || snap.accounts.length === 0) {
    mount(slots.summary);
    rerender(slots.summary, onboarding());
    return;
  }
  mount(slots.summary, slots.plan, overviewChart.el, slots.recent);
  const s = summary(snap, (id) => labelOf(id));
  rerender(slots.summary, h("section", { class: `plan-summary ${s.tone}` }, h("h2", null, s.title), h("p", null, s.story)));
  rerender(slots.plan, planSection(snap));
  fitLabels(slots.plan);
  const recent = state.events.filter((e) => !routine(e));
  rerender(
    slots.recent,
    h(
      "section",
      { class: "card recent" },
      h("div", { class: "panel-head" }, h("h2", null, "Recent activity"), h("button", { class: "link", "data-focus": "see-all", onclick: () => go({ page: "activity" }) }, "See all")),
      h("ol", { class: "activity" }, activityItems(recent, labelOf, 6)),
    ),
  );
}

// ---- account page ----------------------------------------------------------------------

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
      ? `resets in ${until(w.resets_at)}, at ${clock(w.resets_at * 1000)}`
      : w.resets_at === null
        ? "not started"
        : live
          ? "has reset"
          : "no data yet";
  return h(
    "div",
    { class: `meter big ${lvl}` },
    h(
      "div",
      { class: "bar", role: "img", "aria-label": w ? `${pct(used)} used` : reset },
      h("span", { style: { width: `${Math.min(100, used)}%` } }),
      w && h("b", { class: "mark", style: { left: `${Math.min(100, w.threshold)}%` }, title: `cc-rotate swaps at ${pct(w.threshold)}` }),
    ),
    h("div", { class: "meter-text" }, h("strong", { class: "tnum" }, w ? `${pct(used)} used` : "-"), h("span", { class: "muted" }, reset)),
  );
}

function accountLive(a: AccountView): Child[] {
  const others = otherWindows(a);
  const week = weekOf(a);
  const warmStopped = state.snap?.warm_ups_blocked ?? null;
  const warmPaused = state.snap?.warm_ups_paused ?? null;
  const hist = accountHistory?.id === a.id ? accountHistory : null;
  const loading = h("span", { class: "muted" }, hist?.failed ? "couldn't be read" : "Loading…");
  const accounts = state.snap?.accounts ?? [];
  const at = accounts.findIndex((x) => x.id === a.id);
  const count = accounts.length;
  return [
    h(
      "div",
      { class: "meters" },
      h("section", { class: "card" }, h("h2", null, "5-hour window"), meter(fiveOf(a), !!a.last_sample_at)),
      h(
        "section",
        { class: "card" },
        h("h2", null, "Weekly limit"),
        week || !noWeekly(a)
          ? meter(week, !!a.last_sample_at)
          : h("div", { class: "meter-text none" }, h("strong", null, "No weekly limit"), h("span", { class: "muted" }, a.weekly.setting === "no" ? "set by you" : "detected from what Claude Code reports")),
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
      ),
    ),
    h(
      "section",
      { class: "card" },
      h("h2", null, "Details"),
      h(
        "dl",
        { class: "facts" },
        h("dt", null, "Usage"),
        h("dd", { title: readTitle(a) }, readText(a)),
        a.org_name && h("dt", null, "Organization"),
        a.org_name && h("dd", null, a.org_name),
        h("dt", null, "Recent weeks"),
        h("dd", { class: "recent-weeks" }, hist?.hist ? recentWeeks(hist.hist) : loading),
        h("dt", null, "API value"),
        h("dd", { title: "What this use would have cost at API prices, as Claude Code estimates it" }, hist?.hist ? valueText(hist.hist) : loading.cloneNode(true)),
        a.extra_usage && h("dt", null, "Extra usage"),
        a.extra_usage && h("dd", { class: a.extra_usage.enabled ? "warn" : null, title: `As of ${new Date(a.extra_usage.read_at).toLocaleString()}` }, extraText(a.extra_usage)),
        h("dt", null, "Your order"),
        h(
          "dd",
          { class: "order-row" },
          `${ordinal(at + 1)} of ${count}`,
          h("button", { class: "btn small", disabled: at <= 0, "data-focus": "move-up", "data-focus-alt": "move-down", onclick: () => ((state.focus = "move-up"), moveBy(a, -1)) }, "Move up"),
          h("button", { class: "btn small", disabled: at >= count - 1, "data-focus": "move-down", "data-focus-alt": "move-up", onclick: () => ((state.focus = "move-down"), moveBy(a, 1)) }, "Move down"),
        ),
        h("dt", null, "Pick order"),
        h("dd", null, a.rank ? `${ordinal(a.rank)} - ${a.reason}` : `not available${a.reason ? ` - ${a.reason}` : ""}`),
        h("dt", null, "Login updated"),
        h("dd", null, a.login_updated_at ? ago(a.login_updated_at) : "never"),
        a.refresh_expires_at && a.state !== "needs_login" && h("dt", null, "Sign-in ends"),
        a.refresh_expires_at &&
          a.state !== "needs_login" &&
          h("dd", { class: a.sign_in_ends_soon ? "warn" : null, title: new Date(a.refresh_expires_at).toLocaleString() }, a.refresh_expires_at > Date.now() ? `in ${until(a.refresh_expires_at / 1000)}` : "ended"),
        h("dt", null, "Usage check"),
        h("dd", null, checkText(a)),
        a.keep_running && h("dt", null, "Keeping it running"),
        a.keep_running && h("dd", null, warmText(a)),
        a.keep_running && warmStopped && h("dt", null, ""),
        a.keep_running &&
          warmStopped &&
          h(
            "dd",
            { class: "warn" },
            `Stopped because ${warmStopped}. `,
            h(
              "button",
              {
                class: "link",
                "data-focus": "start-again",
                onclick: () =>
                  api
                    .setKeepRunning(a.id, true)
                    .then(() => (say("Keeping 5-hour windows running again"), refresh()))
                    .catch((e) => say(String(e), "error")),
              },
              "Start again",
            ),
          ),
        a.keep_running && !warmStopped && warmPaused && h("dt", null, ""),
        a.keep_running && !warmStopped && warmPaused && h("dd", { class: "warn" }, `Not running now: ${warmPaused}.`),
      ),
    ),
  ];
}

/** The account's own settings, built when its page opens. */
function accountSettings(a: AccountView): HTMLElement {
  const label = h("input", { type: "text", value: a.label, maxlength: 40 });
  const error = h("p", { class: "login-status error", hidden: true });
  const save = h("button", { class: "btn primary" }, "Save changes");
  const remove = h("button", { class: "btn danger-outline", disabled: a.state === "active", title: a.state === "active" ? "Swap to another account first" : null }, "Remove account");
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
  const cap = h("input", { type: "number", min: 1, max: 100, step: "any", value: a.weekly_cap === null ? "" : String(a.weekly_cap), placeholder: String(weeklyThreshold) });
  const useExpiring = h("input", { type: "checkbox", checked: a.use_expiring });
  const keepRunning = h("input", { type: "checkbox", checked: a.keep_running });
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
      if (label.value.trim() !== a.label) {
        await api.renameAccount(a.id, label.value.trim());
        a.label = label.value.trim();
      }
      error.hidden = true;
      say("Saved");
      await refresh();
    } catch (e) {
      error.hidden = false;
      error.textContent = String(e);
    }
  });
  let armed = false;
  remove.addEventListener("click", async () => {
    if (state.snap?.active_id === a.id) {
      error.hidden = false;
      error.textContent = `${a.label} is in use. Swap to another account first.`;
      return;
    }
    if (!armed) {
      armed = true;
      remove.textContent = `Yes, remove ${a.label}`;
      return;
    }
    try {
      await api.removeAccount(a.id);
      say(`Removed ${a.label}`);
      go({ page: "overview" });
      await refresh();
    } catch (e) {
      error.hidden = false;
      error.textContent = String(e);
    }
  });
  return h(
    "section",
    { class: "card" },
    h("h2", null, "Settings for this account"),
    h(
      "div",
      { class: "fields" },
      h("label", null, h("span", null, "Label"), label),
      h("label", null, h("span", null, "Weekly limit"), weekly, h("small", { class: "muted" }, "Detected from the usage Claude Code reports. Saved as soon as you pick.")),
      a.weekly.effective !== "none" &&
        h(
          "label",
          null,
          h("span", null, "Stop using it at weekly usage (%)"),
          cap,
          h("small", { class: "muted" }, `Keeps the rest of its week for you. Empty uses ${weeklyThreshold}% from Settings.`),
        ),
      a.weekly.effective !== "none" && h("label", { class: "check" }, useExpiring, "Use its weekly budget before it resets"),
      h(
        "div",
        { class: "check-field" },
        h("label", { class: "check" }, keepRunning, "Keep its 5-hour window running"),
        h("small", { class: "muted" }, "While it's not in use, starts its next window with a short Haiku request as soon as one ends, so a window is already counting down when it's needed."),
      ),
    ),
    error,
    h("div", { class: "form-actions spread" }, remove, save),
  );
}

function renderAccount(id: number) {
  const a = state.snap?.accounts.find((x) => x.id === id);
  if (!a) {
    mount(slots.live);
    rerender(slots.live, h("p", { class: "muted" }, state.snap ? "This account was removed." : "Loading…"));
    return;
  }
  if (accountForm?.id !== id) accountForm = { id, el: accountSettings(a) };
  mount(slots.live, accountForm.el);
  rerender(slots.live, accountLive(a));
}

// ---- activity, API cost and settings ---------------------------------------------------

function renderActivity() {
  const items = state.events.filter((e) => state.showRoutine || !routine(e));
  mount(slots.activity);
  rerender(slots.activity, h("section", { class: "card" }, h("ol", { class: "activity" }, activityItems(items, labelOf, 300))));
}

/** The API cost page's table: each account's API value over three spans. */
const costTable = h("section", { class: "card" }, h("h2", null, "By account"), h("p", { class: "muted" }, "Loading…"));
let costReads = 0;

function loadCostTable() {
  const read = ++costReads;
  const now = Date.now();
  const spans: [string, number][] = [
    ["24 hours", now - 86_400_000],
    ["7 days", now - 7 * 86_400_000],
    ["30 days", now - 30 * 86_400_000],
  ];
  api
    .costs(spans[2][1])
    .then((costs) => {
      if (read !== costReads) return;
      const accounts = state.snap?.accounts ?? [];
      const sum = (id: number | null, since: number) => costs.filter((c) => (id === null || c.account_id === id) && c.ts + 3_600_000 > since).reduce((s, c) => s + c.usd, 0);
      const rows = accounts.filter((a) => sum(a.id, spans[2][1]) > 0);
      replace(
        costTable,
        h("h2", null, "By account"),
        rows.length === 0
          ? h("p", { class: "muted" }, "No API cost counted in the last 30 days.")
          : h(
              "table",
              { class: "cost-table" },
              h("thead", null, h("tr", null, h("th", null, "Account"), spans.map(([name]) => h("th", { class: "num" }, name)))),
              h(
                "tbody",
                null,
                rows.map((a) => h("tr", null, h("td", null, h("span", { class: "swatch", style: { background: colorOf(a.id) } }), a.label), spans.map(([, since]) => h("td", { class: "num" }, usd(sum(a.id, since)))))),
              ),
              h("tfoot", null, h("tr", null, h("td", null, "Total"), spans.map(([, since]) => h("td", { class: "num" }, usd(rows.reduce((s, a) => s + sum(a.id, since), 0)))))),
            ),
        h("p", { class: "muted cost-note" }, "Counted from the status line of sessions in the cc-rotate profile and other profiles cc-rotate tracks. An hour that started before a span counts in full."),
      );
    })
    .catch((e) => read === costReads && replace(costTable, h("h2", null, "By account"), h("p", { class: "muted" }, `Couldn't read API cost: ${e}`)));
}

function buildSettings(snap: Snapshot): HTMLElement {
  const checks = h("ul", { class: "checks" }, h("li", { class: "muted" }, "Running checks…"));
  const info = h("dl", { class: "facts" });
  const autostart = h("input", { type: "checkbox", id: "autostart" });
  const saved = (key: string, value: string, message: string) =>
    api
      .setSetting(key, value)
      .then(() => (say(message), refresh()))
      .catch((e) => say(String(e), "error"));
  const field = (key: string, text: string, value: number, hint: string, min: number, max: number) => {
    const input = h("input", { type: "number", value: String(value), min, max, step: 1 });
    input.addEventListener("change", () => saved(key, input.value, "Saved"));
    return h("label", null, h("span", null, text), input, h("small", { class: "muted" }, hint));
  };
  const box = (key: keyof Settings, id: string, text: string, on: string, off: string) => {
    const input = h("input", { type: "checkbox", id, checked: Boolean(snap.settings[key]) });
    input.addEventListener("change", () => saved(key, String(input.checked), input.checked ? on : off));
    return h("label", { class: "check", for: id }, input, text);
  };
  api.autostartEnabled().then((on) => (autostart.checked = on)).catch(() => (autostart.disabled = true));
  autostart.addEventListener("change", () =>
    api
      .setAutostart(autostart.checked)
      .then(() => say(autostart.checked ? "cc-rotate will start when you sign in" : "cc-rotate won't start when you sign in"))
      .catch((e) => say(String(e), "error")),
  );
  Promise.all([api.doctor().catch(() => []), api.appInfo().catch(() => null)]).then(([results, about]) => {
    replace(checks, results.map((c) => h("li", { class: c.status }, h("strong", null, c.name), h("span", null, c.detail))));
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
  });
  return h(
    "div",
    { class: "settings" },
    h(
      "section",
      { class: "card" },
      h("h2", null, "When to swap"),
      h(
        "div",
        { class: "fields" },
        field("swap_threshold_5h", "Swap at 5-hour usage (%)", snap.settings.swap_threshold_5h, "Swap before a 5-hour limit is reached.", 1, 100),
        field("swap_threshold_7d", "Swap at weekly usage (%)", snap.settings.swap_threshold_7d, "Also applies to weekly limits on a single model.", 1, 100),
        field("swap_cooldown_s", "Minimum time between swaps (seconds)", snap.settings.swap_cooldown_s, "Applies to swaps at a limit and swaps back.", 0, 3600),
      ),
    ),
    h(
      "section",
      { class: "card" },
      h("h2", null, "Which account comes next"),
      h(
        "div",
        { class: "fields" },
        box("prefer_no_weekly_limit", "prefer-no-weekly", "Use accounts without a weekly limit first, and swap back to one as soon as it's ready", "Accounts without a weekly limit are used first", "Accounts with and without a weekly limit are used alike"),
        box("strict_order", "strict-order", "Pick the next account by my order alone", "The next account is picked by your order alone", "The next account is one whose 5-hour window is already running, when one is"),
        box("use_expiring_weekly", "use-expiring", "Use weekly budget before it resets", "Weekly budget is used before it resets", "Weekly budget is no longer moved ahead"),
        box("avoid_extra_usage", "avoid-extra-usage", "When every account is at a limit, wait instead of paying for extra usage", "Sessions will wait instead of using paid extra usage", "Accounts with extra usage keep working when every account is at a limit"),
      ),
    ),
    h(
      "section",
      { class: "card" },
      h("h2", null, "Usage checks and sign-ins"),
      h(
        "div",
        { class: "fields" },
        box("usage_checks", "usage-checks", "Check usage of accounts not in use", "Usage checks are on", "Usage checks are off"),
        field("usage_check_interval_s", "Check every (seconds)", snap.settings.usage_check_interval_s, "Runs Claude Code's /usage, which doesn't count against limits. Minimum 300.", 300, 86400),
        field("usage_check_max_age_s", "Check before swapping if usage is older than (seconds)", snap.settings.usage_check_max_age_s, "Set to 0 to always check first.", 0, 86400),
        field("login_warning_days", "Warn before a sign-in ends (days)", snap.settings.login_warning_days, "Each sign-in lasts about a month. Set to 0 to turn the warning off.", 0, 30),
      ),
    ),
    h("section", { class: "card" }, h("h2", null, "General"), h("label", { class: "check", for: "autostart" }, autostart, "Start cc-rotate when I sign in")),
    h("section", { class: "card" }, h("h2", null, "Health"), checks),
    h("section", { class: "card" }, h("h2", null, "About"), info),
  );
}

// ---- rendering -------------------------------------------------------------------------

/** A page's sections that stay in place while what's in them is drawn again. */
const slot = () => h("div", { class: "slot" });
const slots = { summary: slot(), plan: slot(), recent: slot(), live: slot(), activity: slot() };
const loadingSettings = h("p", { class: "muted" }, "Loading…");

/**
 * Show `parts` as the page body, leaving it alone when it already shows them: a refresh then
 * never detaches a field, which would take its focus.
 */
function mount(...parts: HTMLElement[]) {
  const same = parts.length === body.children.length && parts.every((p, i) => body.children[i] === p);
  if (!same) replace(body, parts);
}

function renderPage() {
  const r = state.route;
  body.dataset.page = r.page;
  switch (r.page) {
    case "overview":
      renderOverview();
      break;
    case "account":
      renderAccount(r.id);
      break;
    case "activity":
      renderActivity();
      break;
    case "cost":
      mount(costChart.el, costTable);
      break;
    case "settings":
      if (!settingsPage && state.snap) settingsPage = buildSettings(state.snap);
      mount(settingsPage ?? loadingSettings);
      break;
  }
}

// The plan's labels depend on how wide its bars are drawn.
let bodyWidth = 0;
new ResizeObserver(() => {
  if (body.clientWidth === bodyWidth) return;
  bodyWidth = body.clientWidth;
  if (state.route.page === "overview") fitLabels(slots.plan);
}).observe(body);

function render() {
  if (state.dragging) return;
  renderNav();
  renderHead();
  renderBanner();
  renderPage();
}

// ---- dialogs ---------------------------------------------------------------------------

function dialog(title: string, ...children: HTMLElement[]): HTMLDialogElement {
  const d = h("dialog", { class: "modal", "aria-label": title }, h("h2", null, title), ...children);
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
        status.replaceChildren("Finish in your browser. ", h("button", { class: "link", onclick: () => p.url && api.openUrl(p.url) }, "Open the page again"));
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

// ---- data flow -------------------------------------------------------------------------

let refreshing: Promise<void> | null = null;
/** A refresh was asked for while one was reading: it may have missed the change. */
let refreshAgain = false;
async function refresh(): Promise<void> {
  if (refreshing) {
    refreshAgain = true;
    return refreshing;
  }
  refreshing = (async () => {
    try {
      const version = orderVersion;
      const [snap, events] = await Promise.all([api.snapshot(), api.events(300)]);
      // An order the user just changed is still being saved, or was saved after this read began.
      if (pendingOrderSaves > 0 || version !== orderVersion) return;
      state.snap = snap;
      state.events = events;
      colorOf = colorFor(snap.accounts);
      // An account page whose account was removed elsewhere goes back to the overview.
      if (state.route.page === "account" && !snap.accounts.some((a) => state.route.page === "account" && a.id === state.route.id)) state.route = { page: "overview" };
      render();
      if (state.route.page === "overview") await overviewChart.render();
      if (state.route.page === "cost") {
        loadCostTable();
        await costChart.render();
      }
    } catch (e) {
      say(`Couldn't load cc-rotate's data: ${e}`, "error");
    } finally {
      refreshing = null;
      // Also after a read dropped for being older than an order change.
      if (refreshAgain) {
        refreshAgain = false;
        void refresh();
      }
    }
  })();
  return refreshing;
}

onStoreChanged(() => refresh());
onLoginProgress((p) => {
  if (currentLogin && p.id === currentLogin.id) currentLogin.update(p);
});
window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  render();
  overviewChart.refresh();
  costChart.refresh();
});
// Countdowns, effective usage and the plan move with the clock even when nothing is written.
window.setInterval(() => refresh(), 30_000);
render();
refresh();
