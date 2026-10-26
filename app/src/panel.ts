// A chart panel: tabs for its views, the controls, a summary and legend, and the chart itself.
// The overview has one with both views; the API cost page has one with the cost view alone.
import { api, type Snapshot } from "./api";
import { REASONS } from "./activity";
import { Chart, costBuckets, drawCost, drawUsage, usageLines, usd, type Metric, type RangeDays } from "./chart";
import { h, replace } from "./dom";

export type ChartView = "usage" | "cost";

export interface PanelContext {
  snap: () => Snapshot | null;
  color: (id: number) => string;
  labelOf: (id: number | null) => string;
  error: (message: string) => void;
}

const DAY_MS = 86_400_000;
const IN_RANGE: Record<RangeDays, string> = { 1: "in the last 24 hours", 7: "in the last 7 days" };
const VIEW_NAMES: Record<ChartView, string> = { usage: "Usage limits", cost: "API cost" };

function segmented<T extends string>(label: string, options: [T, string][], pick: (value: T) => void) {
  const buttons = options.map(([value, text]) => h("button", { onclick: () => pick(value) }, text));
  return {
    el: h("div", { class: "segmented", role: "group", "aria-label": label }, buttons),
    sync(current: T) {
      buttons.forEach((b, i) => {
        const on = options[i][0] === current;
        b.classList.toggle("on", on);
        b.setAttribute("aria-pressed", String(on));
      });
    },
  };
}

export class ChartPanel {
  readonly el: HTMLElement;
  private view: ChartView;
  private metric: Metric = "weekly";
  private rangeDays: RangeDays = 7;
  /** Renders started so far; a slower read that comes back after a later render started is dropped. */
  private renders = 0;
  private summary = h("div", { class: "chart-summary" });
  private legend = h("div", { class: "legend" });
  private host = h("div", { class: "chart" });
  private empty = h("div", { class: "chart-empty", hidden: true });
  private body: HTMLElement;
  private tabs: HTMLButtonElement[];
  private metricControl;
  private rangeControl;
  private chart: Chart;

  constructor(
    private ctx: PanelContext,
    private views: ChartView[],
    id: string,
  ) {
    this.view = views[0];
    this.body = h("div", { id: `${id}-view`, role: views.length > 1 ? "tabpanel" : null }, this.summary, this.legend, h("div", { class: "chart-wrap" }, this.host, this.empty));
    // The tabs and buttons are built once and updated in place, so a refresh never takes focus
    // from them or has a screen reader announce them again.
    this.tabs = views.map((view, i) =>
      h(
        "button",
        {
          role: "tab",
          id: `${id}-tab-${view}`,
          "aria-controls": `${id}-view`,
          onclick: () => this.pick(view),
          // Arrow keys move between tabs, as in any tab list.
          onkeydown: (e: KeyboardEvent) => {
            const step = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
            if (step === 0) return;
            e.preventDefault();
            const next = (i + step + views.length) % views.length;
            this.pick(views[next]);
            this.tabs[next].focus();
          },
        },
        VIEW_NAMES[view],
      ),
    );
    this.metricControl = segmented<Metric>(
      "Limit",
      [
        ["weekly", "Weekly"],
        ["five_hour", "5-hour"],
      ],
      (value) => {
        this.metric = value;
        this.refresh();
      },
    );
    this.rangeControl = segmented<"1" | "7">(
      "Range",
      [
        ["1", "24 hours"],
        ["7", "7 days"],
      ],
      (value) => {
        this.rangeDays = Number(value) as RangeDays;
        this.refresh();
      },
    );
    const head =
      views.length > 1
        ? h("div", { class: "tabs", role: "tablist", "aria-label": "Charts" }, this.tabs)
        : h("h2", null, "Over time");
    this.el = h(
      "section",
      { class: "card chart-panel" },
      h("div", { class: "panel-head" }, head, h("div", { class: "chart-controls" }, this.metricControl.el, this.rangeControl.el)),
      this.body,
    );
    this.chart = new Chart(this.host);
  }

  private pick(view: ChartView) {
    this.view = view;
    this.refresh();
  }

  /** Draw it again, and say so if its data can't be read. */
  refresh() {
    this.render().catch((e) => this.ctx.error(`Couldn't load the chart: ${e}`));
  }

  private sync() {
    this.tabs.forEach((b, i) => {
      const on = this.views[i] === this.view;
      b.classList.toggle("on", on);
      b.setAttribute("aria-selected", String(on));
      b.tabIndex = on ? 0 : -1;
    });
    if (this.views.length > 1) this.body.setAttribute("aria-labelledby", this.tabs[this.views.indexOf(this.view)].id);
    this.metricControl.el.hidden = this.view !== "usage";
    this.metricControl.sync(this.metric);
    this.rangeControl.sync(String(this.rangeDays) as "1" | "7");
  }

  private message(title: string, detail?: string) {
    this.chart.show(null);
    replace(this.legend);
    replace(this.empty, h("p", null, title), detail && h("p", { class: "muted" }, detail));
    this.empty.hidden = false;
  }

  async render() {
    const render = ++this.renders;
    const snap = this.ctx.snap();
    this.sync();
    if (!snap || snap.accounts.length === 0) {
      replace(this.summary);
      this.message("Charts appear here once a session runs in the cc-rotate profile.");
      return;
    }
    if (this.view === "usage") await this.renderUsage(snap, render);
    else await this.renderCost(snap, render);
  }

  private async renderUsage(snap: Snapshot, render: number) {
    const nowMs = Date.now();
    const sinceMs = nowMs - this.rangeDays * DAY_MS;
    const weekly = this.metric === "weekly";
    // With each account's last reading before the range: the one in force when it starts.
    const [samples, swaps] = await Promise.all([api.series(sinceMs), api.swaps(sinceMs)]);
    if (render !== this.renders) return;
    const lines = usageLines(snap.accounts, samples, this.metric, sinceMs, nowMs);
    const limitName = weekly ? "weekly" : "5-hour";
    const caption = weekly
      ? "How much of each account's weekly limit was used. A line drops to 0 when its limit resets."
      : "How much of each account's 5-hour limit was used. Each climb is one 5-hour window, back to 0 when it resets.";
    replace(this.summary, h("p", { class: "chart-caption" }, caption));
    if (lines.every((l) => l.points.length === 0)) {
      const noLimit = weekly && snap.accounts.every((a) => a.weekly.effective === "none");
      this.message(
        noLimit ? "None of your accounts has a weekly limit." : `No ${limitName} usage read ${IN_RANGE[this.rangeDays]}.`,
        noLimit ? "Their use shows under 5-hour." : "Usage is read from the status line of sessions in the cc-rotate profile, and by usage checks.",
      );
      return;
    }
    const threshold = weekly ? snap.settings.swap_threshold_7d : snap.settings.swap_threshold_5h;
    // An account's own weekly cap stops it sooner.
    const capped = weekly && snap.accounts.some((a) => a.weekly_cap !== null && a.weekly_cap < threshold);
    this.empty.hidden = true;
    replace(
      this.legend,
      lines.map(({ account: a, points }) => {
        const why = points.length > 0 ? null : weekly && a.weekly.effective === "none" ? "no weekly limit" : "no readings";
        return h(
          "span",
          { class: why ? "key off" : "key" },
          h("i", { class: "key-line", style: { background: this.ctx.color(a.id) } }),
          a.label,
          why && h("span", { class: "key-note" }, why),
        );
      }),
      h("span", { class: this.rangeDays === 7 ? "key swap-key tick" : "key swap-key" }, h("i"), "Swap, in the color of the account swapped to"),
      h("span", { class: "key threshold-key" }, h("i"), `cc-rotate swaps at ${threshold}%${capped ? ", or an account's own cap" : ""}`),
    );
    this.chart.show(
      drawUsage({
        lines,
        swaps: swaps.filter((e) => e.account_from !== null && e.account_to !== null),
        color: this.ctx.color,
        describeSwap: (e) => `Swapped to ${this.ctx.labelOf(e.account_to)} ${REASONS[e.reason ?? ""] ?? ""}`.trimEnd(),
        limitName,
        sinceMs,
        nowMs,
        rangeDays: this.rangeDays,
        threshold,
      }),
      `${caption} Over the last ${this.rangeDays === 1 ? "24 hours" : "7 days"}.`,
    );
  }

  private async renderCost(snap: Snapshot, render: number) {
    const nowMs = Date.now();
    const since = costBuckets([], this.rangeDays, nowMs)[0].start;
    const costs = await api.costs(since);
    if (render !== this.renders) return;
    const buckets = costBuckets(costs, this.rangeDays, nowMs);
    const total = buckets.reduce((sum, b) => sum + b.total, 0);
    const byAccount = (id: number) => buckets.reduce((sum, b) => sum + (b.usd.get(id) ?? 0), 0);
    const caption =
      "What each account's use would have cost at API prices, as Claude Code estimates it (what /cost shows). It's a measure of what your plans are worth, not a bill.";
    if (total === 0) {
      replace(this.summary, h("p", { class: "chart-caption" }, caption));
      this.message(
        `No API cost counted ${IN_RANGE[this.rangeDays]}.`,
        "It's counted from the status line of sessions in the cc-rotate profile, and of other profiles cc-rotate tracks.",
      );
      return;
    }
    this.empty.hidden = true;
    replace(
      this.summary,
      h("p", { class: "chart-figure" }, h("strong", null, usd(total)), ` ${IN_RANGE[this.rangeDays]}`),
      h("p", { class: "chart-caption" }, caption),
    );
    replace(
      this.legend,
      snap.accounts
        .filter((a) => byAccount(a.id) > 0)
        .map((a) =>
          h(
            "span",
            { class: "key" },
            h("i", { class: "key-box", style: { background: this.ctx.color(a.id) } }),
            a.label,
            h("span", { class: "key-note" }, usd(byAccount(a.id))),
          ),
        ),
    );
    this.chart.show(
      drawCost({ accounts: snap.accounts, buckets, color: this.ctx.color, rangeDays: this.rangeDays, nowMs }),
      `${usd(total)} at API prices ${IN_RANGE[this.rangeDays]}, by ${this.rangeDays === 1 ? "hour" : "day"} and account.`,
    );
  }
}
