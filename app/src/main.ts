import "@fontsource-variable/schibsted-grotesk";
import "./styles.css";

import {
  api,
  onLoginProgress,
  onStoreChanged,
  type AccountView,
  type EventRow,
  type LoginProgress,
  type Snapshot,
} from "./api";
import { UsageChart, accountColor, type Metric } from "./chart";
import { h, replace, type Child } from "./dom";
import { ago, clock, pct, plural, until } from "./format";

interface UiState {
  snap: Snapshot | null;
  events: EventRow[];
  showRoutine: boolean;
  metric: Metric;
  rangeDays: 1 | 7;
  busy: boolean;
}

const state: UiState = { snap: null, events: [], showRoutine: false, metric: "weekly", rangeDays: 7, busy: false };

const ROUTINE = new Set(["sync", "swap_skipped"]);

// ---- layout ----------------------------------------------------------------------------

const root = document.getElementById("app")!;
const topActions = h("div", { class: "top-actions" });
const banner = h("div", { class: "banner", hidden: true });
const bank = h("div", { class: "bank", role: "list", "aria-label": "Accounts in the pool" });
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
        h("label", { class: "check", for: "routine" }, routineToggle, "Show routine"),
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
      return "Needs login";
  }
}

// ---- top bar and banner ----------------------------------------------------------------

function renderTop() {
  const snap = state.snap;
  const hasAccounts = !!snap && snap.accounts.length > 0;
  replace(
    topActions,
    h("button", { class: "btn quiet", onclick: openHealth }, "Health"),
    hasAccounts &&
      h(
        "button",
        {
          class: "btn",
          disabled: !snap!.pool.eligible,
          title: snap!.pool.eligible ? "Swap to the account with the most room" : "No other account is ready",
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
  if (needs.length > 0) {
    banner.hidden = false;
    banner.className = "banner danger";
    replace(
      banner,
      h(
        "span",
        null,
        needs.length === 1
          ? `${needs[0].label} needs a new login before it can be used again.`
          : `${needs.map((a) => a.label).join(", ")} need new logins before they can be used again.`,
      ),
      h("button", { class: "btn small", onclick: () => openLogin(needs[0]) }, `Log in to ${needs[0].label}`),
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
        "Every account is near a limit, paused, or waiting for a login. ",
        next ? `The next one frees up in ${until(next)}.` : "",
      ),
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
          "Each account signs in through Claude Code's own login page. cc-rotate keeps the logins encrypted on this computer and swaps the cc-rotate profile between them as limits approach.",
        ),
        h("button", { class: "btn primary", onclick: () => openLogin(null) }, "Add account"),
      ),
    );
    replace(summary);
    return;
  }
  const t = snap.settings;
  replace(
    bank,
    snap.accounts.map((a, i) => {
      const weekLeft = Math.max(0, 100 - a.usage.weekly);
      const fiveLeft = Math.max(0, 100 - a.usage.five_hour);
      const blocked = a.usage.weekly >= t.swap_threshold_7d || a.usage.five_hour >= t.swap_threshold_5h;
      const tone =
        a.state === "needs_login" ? "dead" : a.state === "paused" ? "paused" : blocked ? "over" : a.state === "active" ? "active" : level(a.usage.weekly, t.swap_threshold_7d);
      const title = [
        `${a.label}${a.email ? ` (${a.email})` : ""}`,
        `${stateText(a)}`,
        `Week: ${pct(a.usage.weekly)} used`,
        `5-hour: ${pct(a.usage.five_hour)} used`,
      ].join("\n");
      return h(
        "div",
        { class: `cell ${tone}`, role: "listitem", title, style: { "--series": accountColor(i) } },
        h("div", { class: "tank" }, h("div", { class: "fill", style: { height: `${a.state === "needs_login" ? 0 : weekLeft}%` } })),
        h("div", { class: "five", "aria-hidden": "true" }, h("span", { style: { width: `${a.state === "needs_login" ? 0 : fiveLeft}%` } })),
        h("div", { class: "cell-label" }, a.label),
        h(
          "div",
          { class: "cell-sub tnum" },
          a.state === "needs_login" ? "Log in" : a.state === "paused" ? "Paused" : a.last_sample ? `${pct(weekLeft)} left` : "No reading",
        ),
      );
    }),
  );

  const active = snap.accounts.find((a) => a.id === snap.active_id);
  const usable = snap.pool.usable;
  const unavailable = snap.accounts.length - usable;
  const left = snap.pool.weekly_accounts_left;
  const swapEvent = state.events.find((e) => e.kind === "swap" && e.account_to === snap.active_id);
  replace(
    summary,
    h("p", { class: "headline" }, `${left.toFixed(1)} of ${usable} accounts' weekly use left`),
    h(
      "dl",
      { class: "facts" },
      h("dt", null, "Using"),
      h("dd", null, active ? active.label : "No account", active && swapEvent ? h("span", { class: "muted" }, ` since ${clock(swapEvent.ts)}`) : null),
      h("dt", null, "Ready next"),
      h("dd", null, snap.pool.eligible ? plural(snap.pool.eligible, "account") : "None"),
      unavailable > 0 && h("dt", null, "Unavailable"),
      unavailable > 0 && h("dd", null, `${unavailable}`, h("span", { class: "muted" }, " paused or logged out")),
      h("dt", null, "Swaps"),
      h("dd", null, String(snap.pool.swaps_24h), h("span", { class: "muted" }, " in the last 24 hours")),
    ),
  );
}

// ---- accounts table --------------------------------------------------------------------

function meter(used: number, resetsAt: number | null | undefined, threshold: number, live: boolean) {
  const lvl = level(used, threshold);
  const reset = resetsAt && resetsAt * 1000 > Date.now() ? `resets in ${until(resetsAt)}` : live ? "window reset" : "no reading yet";
  return h(
    "div",
    { class: `meter ${lvl}` },
    h("div", { class: "bar", role: "img", "aria-label": `${pct(used)} used` }, h("span", { style: { width: `${Math.min(100, used)}%` } })),
    h("div", { class: "meter-text" }, h("strong", { class: "tnum" }, pct(used)), h("span", { class: "muted" }, reset)),
  );
}

function renderAccounts() {
  const snap = state.snap;
  if (!snap || snap.accounts.length === 0) {
    replace(accountsBody, h("p", { class: "muted empty" }, "No accounts yet."));
    return;
  }
  const t = snap.settings;
  const rows = snap.accounts.map((a, i) => {
    const s = a.last_sample;
    const actions = [];
    if (a.state === "parked")
      actions.push(h("button", { class: "link", onclick: () => act(() => api.swapNow(a.id), () => `Swapped to ${a.label}`) }, "Use now"));
    if (a.state === "paused")
      actions.push(h("button", { class: "link", onclick: () => act(() => api.setPaused(a.id, false), () => `Resumed ${a.label}`) }, "Resume"));
    if (a.state === "needs_login") actions.push(h("button", { class: "link", onclick: () => openLogin(a) }, "Log in again"));
    actions.push(h("button", { class: "link quiet-link", onclick: () => openManage(a), "aria-label": `More for ${a.label}` }, "More"));

    return h(
      "tr",
      { class: a.state === "active" ? "is-active" : "" },
      h(
        "td",
        null,
        h(
          "div",
          { class: "who" },
          h("span", { class: "swatch", style: { background: accountColor(i) }, "aria-hidden": "true" }),
          h("div", null, h("div", { class: "who-label" }, a.label), h("div", { class: "muted who-email" }, a.email ?? "")),
        ),
      ),
      h(
        "td",
        null,
        h("span", { class: `badge ${a.state}${a.state === "parked" && !a.eligible ? " resting" : ""}` }, stateText(a)),
        h("div", { class: "muted updated", title: s ? new Date(s.ts).toLocaleString() : "" }, s ? `read ${ago(s.ts)}` : "never read"),
      ),
      h("td", null, meter(a.usage.five_hour, s?.five_resets_at, t.swap_threshold_5h, !!s)),
      h("td", null, meter(a.usage.weekly, s?.week_resets_at, t.swap_threshold_7d, !!s)),
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
        h("tr", null, h("th", null, "Account"), h("th", null, "State"), h("th", null, "5-hour limit"), h("th", null, "Weekly limit"), h("th", null, h("span", { class: "sr" }, "Actions"))),
      ),
      h("tbody", null, rows),
    ),
  );
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
    snap.accounts.map((a, i) => h("span", { class: "key" }, h("i", { style: { background: accountColor(i) } }), a.label)),
    h("span", { class: "key swap-key" }, h("i"), "Swap"),
  );
  chart.render({
    accounts: snap.accounts,
    samples,
    swaps: state.events.filter((e) => e.kind === "swap" && e.account_from !== null && e.ts >= sinceMs),
    metric: state.metric,
    sinceMs,
    threshold: state.metric === "weekly" ? snap.settings.swap_threshold_7d : snap.settings.swap_threshold_5h,
  });
}

// ---- activity rail ---------------------------------------------------------------------

const REASONS: Record<string, string> = {
  threshold_5h: "at the 5-hour limit",
  threshold_7d: "at the weekly limit",
  stop_failure: "after a turn hit a limit",
  dead_login: "because its login stopped working",
  manual: "by hand",
  login: "after its first login",
};

const nb = (text: string) => h("span", { class: "nb" }, text);

function describe(e: EventRow): { parts: Child[]; tone?: "warn" | "danger" } {
  const from = nb(labelOf(e.account_from));
  const to = nb(labelOf(e.account_to));
  const detail = (k: string) => (e.detail?.[k] as string | undefined) ?? "";
  switch (e.kind) {
    case "swap":
      return e.account_from
        ? { parts: ["Swapped from ", from, " to ", to, ` ${REASONS[e.reason ?? ""] ?? ""}`.trimEnd()] }
        : { parts: ["Started using ", to] };
    case "swap_skipped":
      return { parts: [`Swap skipped: ${detail("why")}`] };
    case "login":
      return { parts: e.reason === "added" ? ["Added ", to] : ["Logged ", to, " in again"] };
    case "sync":
      return { parts: ["Saved a refreshed login for ", from] };
    case "dead_login":
      return { parts: [from, "'s login stopped working"], tone: "danger" };
    case "pool_exhausted":
      return { parts: ["No other account was ready to take over"], tone: "warn" };
    case "adopted_login":
      return { parts: ["Picked up a /login as ", to] };
    case "foreign_login":
      return { parts: ["The profile was signed in to an account outside the pool"], tone: "warn" };
    case "state":
      if (e.reason === "removed") return { parts: ["Removed ", nb(detail("label") || "an account")] };
      if (e.reason === "paused") return { parts: ["Paused ", from] };
      if (e.reason === "parked") return { parts: ["Resumed ", from] };
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
  const items = state.events.filter((e) => state.showRoutine || !ROUTINE.has(e.kind));
  if (items.length === 0) {
    replace(activity, h("li", { class: "muted empty" }, "Swaps, logins and problems show up here."));
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
  const label = h("input", { type: "text", placeholder: "Defaults to the email name", maxlength: 40, autocomplete: "off" });
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
    h("label", null, h("span", null, "Label"), label, h("small", { class: "muted" }, "Optional. A short name like work or team-3.")),
    h("p", { class: "muted" }, "Your browser signs in as whichever claude.ai account it is logged in to. Switch accounts there first if needed."),
  );
  const form = account
    ? h("p", null, `Sign in as ${account.email ?? account.label} in the browser window that opens. If the browser is logged in to a different claude.ai account, switch accounts there first.`)
    : h("div", null, found, fields);

  const d = dialog(account ? `Log in to ${account.label}` : "Add an account", form, steps, status, codeRow, h("div", { class: "dialog-actions" }, cancel, start));

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
                  h("div", { class: "muted who-email" }, `${p.name} profile${p.subscription_type ? `, ${p.subscription_type}` : ""}`),
                ),
                p.in_pool_as
                  ? h("span", { class: "muted found-state" }, `In the pool as ${p.in_pool_as}`)
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
          h("h3", null, "Or another account"),
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
        status.textContent = "Starting Claude Code's sign-in.";
        break;
      case "browser": {
        setSteps(1);
        status.replaceChildren(
          "Finish signing in in your browser. ",
          h("button", { class: "link", onclick: () => p.url && api.openUrl(p.url) }, "Open the page again"),
        );
        const code = h("input", { type: "text", placeholder: "Paste the code here", autocomplete: "off" });
        replace(
          codeRow,
          h("label", null, h("span", null, "If the browser shows a code instead of finishing, paste it here"), code),
          h("button", { class: "btn small", onclick: () => code.value.trim() && api.submitLoginCode(p.id, code.value).catch((e) => say(String(e), "error")) }, "Submit code"),
        );
        codeRow.hidden = false;
        break;
      }
      case "importing":
        setSteps(2);
        codeRow.hidden = true;
        status.textContent = "Saving the login.";
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
        status.textContent = p.message ?? "The sign-in did not finish.";
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
  const quick = h(
    "div",
    { class: "quick" },
    a.state === "parked" && h("button", { class: "btn small", onclick: run(() => api.swapNow(a.id), `Swapped to ${a.label}`) }, "Use now"),
    a.state === "parked" && h("button", { class: "btn small", onclick: run(() => api.setPaused(a.id, true), `Paused ${a.label}`) }, "Pause"),
    a.state === "paused" && h("button", { class: "btn small", onclick: run(() => api.setPaused(a.id, false), `Resumed ${a.label}`) }, "Resume"),
    h("button", { class: "btn small", onclick: () => (d.close(), openLogin(a)) }, a.state === "needs_login" ? "Log in again" : "Re-login"),
  );
  const details = h(
    "dl",
    { class: "facts info" },
    h("dt", null, "Email"),
    h("dd", null, a.email ?? "unknown"),
    a.org_name && h("dt", null, "Organization"),
    a.org_name && h("dd", null, a.org_name),
    a.subscription_type && h("dt", null, "Plan"),
    a.subscription_type && h("dd", null, a.subscription_type),
    h("dt", null, "Login saved"),
    h("dd", null, a.login_updated_at ? ago(a.login_updated_at) : "never"),
  );
  const d = dialog(
    a.label,
    details,
    quick,
    h("div", { class: "fields" }, h("label", null, h("span", null, "Label"), label)),
    error,
    h("div", { class: "dialog-actions spread" }, remove, h("div", null, close, save)),
  );
  close.addEventListener("click", () => d.close());
  save.addEventListener("click", async () => {
    try {
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
      remove.textContent = `Remove ${a.label} and its login`;
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

async function openHealth() {
  const checks = h("ul", { class: "checks" }, h("li", { class: "muted" }, "Running checks."));
  const info = h("dl", { class: "facts info" });
  const settingsForm = h("div", { class: "fields" });
  const autostart = h("input", { type: "checkbox", id: "autostart" });
  const d = dialog(
    "Health and settings",
    checks,
    h("h3", null, "Settings"),
    settingsForm,
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
        api.setSetting(key, input.value).then(() => (say(`${text} saved`), refresh())).catch((e) => say(String(e), "error")),
      );
      return h("label", null, h("span", null, text), input, h("small", { class: "muted" }, hint));
    };
    replace(
      settingsForm,
      field("swap_threshold_5h", "Swap at 5-hour usage (%)", snap.settings.swap_threshold_5h, "Swap before the 5-hour limit is reached.", 1, 100),
      field("swap_threshold_7d", "Swap at weekly usage (%)", snap.settings.swap_threshold_7d, "Swap before the weekly limit is reached.", 1, 100),
      field("swap_cooldown_s", "Minimum time between swaps (seconds)", snap.settings.swap_cooldown_s, "Limit swaps caused by usage readings.", 0, 3600),
    );
  }
  api.autostartEnabled().then((on) => (autostart.checked = on)).catch(() => (autostart.disabled = true));
  autostart.addEventListener("change", () =>
    api.setAutostart(autostart.checked).then(() => say(autostart.checked ? "cc-rotate will start when you sign in" : "cc-rotate will not start on its own")).catch((e) => say(String(e), "error")),
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
      h("dt", null, "Profile"),
      h("dd", { class: "path" }, about.profile_dir),
      h("dt", null, "Data"),
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
      state.snap = snap;
      state.events = events;
      renderTop();
      renderOverview();
      renderAccounts();
      renderActivity();
      await renderChart();
    } catch (e) {
      say(`Could not read the cc-rotate store: ${e}`, "error");
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
