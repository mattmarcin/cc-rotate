// The chart panel's two views, drawn as SVG: how much of a usage limit each account used over
// time (stepped lines, swaps marked along the top), and what each account's use would have cost
// at API prices (bars stacked per account, one per hour or day).
import type { Account, EventRow, HourlyCost, Sample } from "./api";
import { h, replace, type Child } from "./dom";

export type Metric = "weekly" | "five_hour";
export type RangeDays = 1 | 7;

const LIGHT = ["#0F7C86", "#4C5BD4", "#9B3F8F", "#6E7F1F", "#1F7FBF", "#A65B2A", "#5A6B8C", "#C2457A"];
const DARK = ["#3FB6BF", "#8C97F2", "#D07CC4", "#A9BD52", "#5CB2EE", "#E09A63", "#93A3C2", "#EB86B0"];

const dark = () => window.matchMedia("(prefers-color-scheme: dark)").matches;

export function accountColor(index: number): string {
  const palette = dark() ? DARK : LIGHT;
  return palette[index % palette.length];
}

/** A color per account that stays the same when the order changes: by the order accounts were added. */
export function colorFor(accounts: { id: number }[]): (id: number) => string {
  const ids = accounts.map((a) => a.id).sort((a, b) => a - b);
  return (id) => accountColor(Math.max(0, ids.indexOf(id)));
}

/** The window each chart metric shows. */
const WINDOW: Record<Metric, string> = { weekly: "seven_day", five_hour: "five_hour" };


const HOUR_MS = 3_600_000;

// ---- drawing ---------------------------------------------------------------------------

const SVG_NS = "http://www.w3.org/2000/svg";

function s<K extends keyof SVGElementTagNameMap>(
  tag: K,
  attrs: Record<string, string | number | null | undefined>,
  ...children: (SVGElement | string)[]
): SVGElementTagNameMap[K] {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value !== null && value !== undefined) el.setAttribute(key, String(value));
  }
  el.append(...children);
  return el;
}

interface Box {
  left: number;
  top: number;
  width: number;
  height: number;
}

interface Frame {
  svg: SVGSVGElement;
  width: number;
  height: number;
  /** Where the data goes, inside the axis labels. */
  plot: Box;
}

interface HoverResult {
  /** Where the crosshair goes, if the view has one. */
  crosshair: number | null;
  /** Where the tooltip points to. */
  x: number;
  tip: Child;
  marks?: SVGElement[];
}

/** What the pointer at `x` (px within the chart) shows; `null` as it leaves. */
type Hover = (x: number | null) => HoverResult | null;

/** Draws a view into a frame and returns what hovering it shows. */
export type Draw = (frame: Frame) => Hover | null;

/** Room around the plot: swap markers above it, value labels left of it, time labels below. */
const PAD = { top: 14, right: 14, bottom: 26, left: 46 };

/** One chart host that shows whichever view it was last given, redrawn as it resizes. */
export class Chart {
  private svg = s("svg", { class: "chart-svg", role: "img" });
  private tip = h("div", { class: "chart-tip", hidden: true, "aria-hidden": "true" });
  private overlay = s("g", { class: "chart-overlay" });
  private draw: Draw | null = null;
  private hover: Hover | null = null;
  private plot: Box = { left: 0, top: 0, width: 0, height: 0 };
  private pointerX: number | null = null;

  constructor(private host: HTMLElement) {
    host.append(this.svg, this.tip);
    new ResizeObserver(() => this.redraw()).observe(host);
    this.svg.addEventListener("pointermove", (e) => {
      this.pointerX = e.clientX - this.svg.getBoundingClientRect().left;
      this.showHover();
    });
    this.svg.addEventListener("pointerleave", () => {
      this.pointerX = null;
      this.showHover();
    });
  }

  show(draw: Draw | null, label = "") {
    this.draw = draw;
    this.svg.setAttribute("aria-label", label);
    this.redraw();
  }

  private redraw() {
    const width = this.host.clientWidth;
    const height = this.host.clientHeight;
    this.svg.replaceChildren();
    this.svg.setAttribute("width", String(width));
    this.svg.setAttribute("height", String(height));
    this.svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
    this.plot = {
      left: PAD.left,
      top: PAD.top,
      width: Math.max(0, width - PAD.left - PAD.right),
      height: Math.max(0, height - PAD.top - PAD.bottom),
    };
    this.hover = this.draw && width > 0 ? this.draw({ svg: this.svg, width, height, plot: this.plot }) : null;
    this.overlay.replaceChildren();
    this.svg.append(this.overlay);
    // A refresh keeps the tooltip where the pointer is, with the new numbers.
    this.showHover();
  }

  private showHover() {
    this.overlay.replaceChildren();
    const x = this.pointerX;
    const inside = x !== null && x >= this.plot.left - 4 && x <= this.plot.left + this.plot.width + 4;
    const got = this.hover?.(inside ? x : null) ?? null;
    if (!got) {
      this.tip.hidden = true;
      return;
    }
    const { top, height } = this.plot;
    if (got.crosshair !== null) {
      const cx = Math.round(got.crosshair) + 0.5;
      this.overlay.append(s("line", { class: "crosshair", x1: cx, x2: cx, y1: top, y2: top + height }));
    }
    this.overlay.append(...(got.marks ?? []));
    replace(this.tip, got.tip);
    this.tip.hidden = false;
    // Beside the point, on whichever side has room.
    const w = this.tip.offsetWidth;
    const right = got.x + 14;
    const left = right + w <= this.host.clientWidth ? right : Math.max(0, got.x - 14 - w);
    this.tip.style.left = `${left}px`;
    // From the top of the plot, or higher (over the legend) when it is taller than the chart.
    this.tip.style.top = `${Math.min(top, this.host.clientHeight - this.tip.offsetHeight)}px`;
  }
}

// ---- time axis --------------------------------------------------------------------------

/** "3 PM"; "3:30 PM" where the clock's hours are not epoch hours (UTC+5:30). */
const hourLabel = (d: Date) => d.toLocaleTimeString([], { hour: "numeric", minute: d.getMinutes() === 0 ? undefined : "2-digit" });
const weekday = (d: Date) => d.toLocaleDateString([], { weekday: "short" });

/**
 * When the local day `days` after the one holding `ms` starts, from its calendar date: midnight,
 * or 1 AM where a daylight saving change skips midnight, without carrying that hour to other days.
 */
export function dayStart(ms: number, days = 0): number {
  const d = new Date(ms);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + days).getTime();
}

/** Starts of the local days after `fromMs` and before `toMs`. */
function midnights(fromMs: number, toMs: number): number[] {
  const out: number[] = [];
  for (let i = 1, t = dayStart(fromMs, 1); t < toMs; t = dayStart(fromMs, ++i)) out.push(t);
  return out;
}

/** "3:04 PM" today, "Thu 3:04 PM" this week, "Oct 12, 3:04 PM" before: always with the time. */
function stamp(ms: number): string {
  const at = new Date(ms);
  const time = at.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const now = Date.now();
  if (ms >= dayStart(now)) return time;
  if (ms >= dayStart(now, -6)) return `${weekday(at)} ${time}`;
  return `${at.toLocaleDateString([], { month: "short", day: "numeric" })}, ${time}`;
}

function text(x: number, y: number, label: string, cls: string, anchor = "middle"): SVGTextElement {
  return s("text", { x, y, class: cls, "text-anchor": anchor }, label);
}

/**
 * The time axis under the usage lines: over 24 hours every third hour, the day's name at
 * midnight; over 7 days each day's name in the middle of its span. Midnights get a gridline.
 */
function timeAxis(f: Frame, fromMs: number, toMs: number, rangeDays: RangeDays, x: (t: number) => number) {
  const { svg, plot } = f;
  const right = plot.left + plot.width;
  const bottom = plot.top + plot.height;
  const y = bottom + 17;
  const days = midnights(fromMs, toMs);
  for (const t of days) {
    const gx = Math.round(x(t)) + 0.5;
    svg.append(s("line", { class: "grid day", x1: gx, x2: gx, y1: plot.top, y2: bottom }));
  }
  if (rangeDays === 1) {
    const d = new Date(fromMs);
    d.setMinutes(0, 0, 0);
    for (d.setHours(d.getHours() + 1); d.getTime() < toMs; d.setHours(d.getHours() + 1)) {
      const hour = d.getHours();
      if (hour % 3 !== 0) continue;
      const tx = x(d.getTime());
      // A label must fit inside the chart.
      if (tx < plot.left + 16 || tx > right - 16) continue;
      svg.append(text(tx, y, hour === 0 ? weekday(d) : hourLabel(d), hour === 0 ? "tick strong" : "tick"));
    }
    return;
  }
  const edges = [fromMs, ...days, toMs];
  for (let i = 0; i + 1 < edges.length; i++) {
    const a = x(edges[i]);
    const b = x(edges[i + 1]);
    // Too little of the day shows to name it.
    if (b - a < 34) continue;
    const today = i + 1 === edges.length - 1;
    svg.append(text((a + b) / 2, y, today ? "Today" : weekday(new Date(edges[i])), today ? "tick strong" : "tick"));
  }
}

// ---- usage lines ------------------------------------------------------------------------

export interface UsageLine<A extends Account = Account> {
  account: A;
  /** [epoch ms, percent] from the first reading on: each value holds until the next point. */
  points: [number, number][];
}

/**
 * Each account's line: its readings of the metric's window, from the one in force when the range
 * starts (a reading before it whose window had not reset by then, else 0 if it had), with a drop to
 * 0 wherever the window reset between readings, and the last value held until now. A reading whose
 * window had already reset when it was taken (an idle session repeating its last numbers) is 0,
 * as the scheduler counts it. An account without a reading of the window has no line.
 */
export function usageLines<A extends Account>(accounts: A[], samples: Sample[], metric: Metric, sinceMs: number, nowMs: number): UsageLine<A>[] {
  return accounts.map((account) => {
    const readings = samples
      .filter((s) => s.account_id === account.id)
      .flatMap((s) => {
        const w = s.windows.find((x) => x.name === WINDOW[metric]);
        if (!w) return [];
        const resetsMs = w.resets_at === null ? null : w.resets_at * 1000;
        return resetsMs !== null && resetsMs <= s.ts ? [{ ts: s.ts, pct: 0, resetsMs: null }] : [{ ts: s.ts, pct: w.pct, resetsMs }];
      });
    let first = readings.findIndex((r) => r.ts >= sinceMs);
    if (first === -1) first = readings.length;
    const before = readings[first - 1];
    const used = readings.slice(first);
    if (before) {
      const open = before.resetsMs === null || before.resetsMs > sinceMs;
      used.unshift({ ts: sinceMs, pct: open ? before.pct : 0, resetsMs: open ? before.resetsMs : null });
    }
    const points: [number, number][] = [];
    used.forEach((r, i) => {
      points.push([r.ts, r.pct]);
      const next = i + 1 < used.length ? used[i + 1].ts : nowMs;
      if (r.resetsMs !== null && r.resetsMs > r.ts && r.resetsMs < next) points.push([r.resetsMs, 0]);
    });
    if (points.length > 0) points.push([nowMs, points[points.length - 1][1]]);
    return { account, points };
  });
}

/** The value a stepped line holds at `t`, or null before it starts. */
function valueAt(points: [number, number][], t: number): number | null {
  let value: number | null = null;
  for (const [pt, v] of points) {
    if (pt > t) break;
    value = v;
  }
  return value;
}

export interface UsageInput {
  lines: UsageLine[];
  swaps: EventRow[];
  color: (id: number) => string;
  /** What a swap's marker says in the tooltip: "Swapped to work at the 5-hour limit". */
  describeSwap: (e: EventRow) => string;
  /** "weekly", "5-hour": what the values are a share of. */
  limitName: string;
  sinceMs: number;
  nowMs: number;
  rangeDays: RangeDays;
  threshold: number;
}

export interface PlacedSwap {
  e: EventRow;
  x: number;
}

/** Swaps within this (px) of the first in a group share its marker; no marker spans more. */
const SWAP_GAP = 14;
/** Swaps a tooltip lists before "and N more". */
const SWAPS_LISTED = 4;
/** How near (px) the pointer has to be to a 7-day tick for the tooltip to list its swap. */
const TICK_REACH = 7;

/** The swaps within `reach` px of the pointer at `px`, oldest first. */
export function swapsNear(swaps: PlacedSwap[], px: number, reach = TICK_REACH): PlacedSwap[] {
  return swaps.filter((sw) => Math.abs(sw.x - px) <= reach).sort((a, b) => a.x - b.x);
}

/** One marker along the top: a triangle for one swap, a pill with a count for several. */
export interface SwapMarker {
  swaps: PlacedSwap[];
  x: number;
  width: number;
}

function swapMarker(swaps: PlacedSwap[]): SwapMarker {
  return {
    swaps,
    x: (swaps[0].x + swaps[swaps.length - 1].x) / 2,
    width: swaps.length === 1 ? 8 : 10 + 6 * String(swaps.length).length,
  };
}

/**
 * Swaps grouped into markers along the top, between `left` and `right` (px): a swap less than
 * `SWAP_GAP` after the first in a group joins it. A marker that would touch the one before it moves
 * over (and back from the right edge), so markers never overlap and none spans much time.
 */
export function swapMarkers(swaps: PlacedSwap[], left: number, right: number): SwapMarker[] {
  const groups: PlacedSwap[][] = [];
  for (const sw of [...swaps].sort((a, b) => a.x - b.x)) {
    const last = groups[groups.length - 1];
    if (last && sw.x - last[0].x < SWAP_GAP) last.push(sw);
    else groups.push([sw]);
  }
  const markers = groups.map(swapMarker);
  const apart = (a: SwapMarker, b: SwapMarker) => (a.width + b.width) / 2 + 2;
  for (let i = 1; i < markers.length; i++) markers[i].x = Math.max(markers[i].x, markers[i - 1].x + apart(markers[i - 1], markers[i]));
  const last = markers[markers.length - 1];
  if (last) last.x = Math.min(last.x, right - last.width / 2);
  for (let i = markers.length - 2; i >= 0; i--) markers[i].x = Math.min(markers[i].x, markers[i + 1].x - apart(markers[i], markers[i + 1]));
  for (const m of markers) m.x = Math.max(m.x, left + m.width / 2 - 4);
  return markers;
}

export function drawUsage(input: UsageInput): Draw {
  return (f) => {
    const { svg, plot } = f;
    const { sinceMs, nowMs } = input;
    const span = Math.max(1, nowMs - sinceMs);
    const x = (t: number) => plot.left + ((Math.min(Math.max(t, sinceMs), nowMs) - sinceMs) / span) * plot.width;
    const y = (v: number) => plot.top + (1 - Math.min(Math.max(v, 0), 100) / 100) * plot.height;
    const right = plot.left + plot.width;

    for (const v of [0, 25, 50, 75, 100]) {
      const gy = Math.round(y(v)) + 0.5;
      svg.append(
        s("line", { class: v === 0 ? "axis" : "grid", x1: plot.left, x2: right, y1: gy, y2: gy }),
        text(plot.left - 8, gy + 4, `${v}%`, "tick", "end"),
      );
    }
    timeAxis(f, sinceMs, nowMs, input.rangeDays, x);
    const ty = Math.round(y(input.threshold)) + 0.5;
    svg.append(s("line", { class: "threshold", x1: plot.left, x2: right, y1: ty, y2: ty }));

    for (const line of input.lines) {
      if (line.points.length === 0) continue;
      const [[t0, v0], ...rest] = line.points;
      let d = `M${x(t0).toFixed(1)},${y(v0).toFixed(1)}`;
      for (const [t, v] of rest) d += `H${x(t).toFixed(1)}V${y(v).toFixed(1)}`;
      svg.append(s("path", { class: "line", d, stroke: input.color(line.account.id) }));
    }

    const swaps = input.swaps
      .filter((e) => e.ts >= sinceMs && e.ts <= nowMs)
      .map((e) => ({ e, x: x(e.ts) }));
    const swapColor = (e: EventRow) => (input.lines.some((l) => l.account.id === e.account_to) ? input.color(e.account_to!) : null);
    // Over 7 days, a thin tick at each swap's own time: ticks that overlap show how dense swaps
    // were. Over 24 hours, a triangle per swap, grouped only where triangles would collide.
    const ticks = input.rangeDays === 7;
    const markers = ticks ? [] : swapMarkers(swaps, plot.left, right + PAD.right);
    if (ticks) {
      for (const { e, x: sx } of swaps) {
        const color = swapColor(e);
        svg.append(
          s("rect", {
            class: "swap-mark",
            x: Math.round(sx) - 1,
            y: plot.top - 11,
            width: 2,
            height: 9,
            style: color ? `fill: ${color}` : null,
          }),
        );
      }
    }
    for (const m of markers) {
      if (m.swaps.length === 1) {
        const color = swapColor(m.swaps[0].e);
        svg.append(
          s("path", {
            class: "swap-mark",
            d: `M${m.x - 4},${plot.top - 10}H${m.x + 4}L${m.x},${plot.top - 3}Z`,
            style: color ? `fill: ${color}` : null,
          }),
        );
        continue;
      }
      svg.append(
        s("rect", { class: "swap-group", x: m.x - m.width / 2, y: plot.top - 13, width: m.width, height: 11, rx: 5.5 }),
        text(m.x, plot.top - 4.5, String(m.swaps.length), "swap-count"),
      );
    }

    return (px) => {
      if (px === null) return null;
      const t = sinceMs + ((Math.min(Math.max(px, plot.left), right) - plot.left) / Math.max(1, plot.width)) * span;
      const cx = x(t);
      const rows: Child[] = [];
      const marks: SVGElement[] = [];
      for (const line of input.lines) {
        const v = valueAt(line.points, t);
        if (v === null) continue;
        const color = input.color(line.account.id);
        marks.push(s("circle", { class: "dot", cx, cy: y(v), r: 4, fill: color }));
        rows.push([
          h("i", { class: "tip-line", style: { background: color } }),
          h("b", null, `${Math.round(v)}%`),
          h("span", null, line.account.label),
        ]);
      }
      // The swaps under the pointer: near its ticks, or the nearest marker's.
      const reach = (m: SwapMarker) =>
        Math.abs(m.x - px) <= Math.max(6, m.width / 2 + 2) || (px >= m.swaps[0].x - 4 && px <= m.swaps[m.swaps.length - 1].x + 4);
      const under = ticks
        ? swapsNear(swaps, px)
        : (markers.filter(reach).sort((a, b) => Math.abs(a.x - px) - Math.abs(b.x - px))[0]?.swaps ?? []);
      // One swap: the crosshair goes to it. Several: it stays with the pointer, and the tooltip
      // lists them.
      const near = under.length === 1 ? under[0] : null;
      const group = under.length > 1 ? under : null;
      return {
        crosshair: near ? near.x : cx,
        x: near ? near.x : cx,
        tip: [
          h("div", { class: "tip-head" }, stamp(near ? near.e.ts : t)),
          near && h("div", { class: "tip-note" }, input.describeSwap(near.e)),
          group && [
            h("div", { class: "tip-note" }, `${group.length} swaps here`),
            // One grid, so the times and descriptions line up in columns.
            h(
              "div",
              { class: "tip-swaps" },
              group
                .slice(0, SWAPS_LISTED)
                .map(({ e }) => [
                  // Keyed like the chart's own markers: a tick over 7 days, a triangle over 24 hours.
                  ticks
                    ? h("i", { class: "tip-mark tick", style: { background: swapColor(e) ?? "" } })
                    : h("i", { class: "tip-mark", style: { borderTopColor: swapColor(e) ?? "" } }),
                  h("span", { class: "muted" }, stamp(e.ts)),
                  h("span", null, input.describeSwap(e)),
                ]),
            ),
            group.length > SWAPS_LISTED && h("div", { class: "tip-note" }, `and ${group.length - SWAPS_LISTED} more`),
          ],
          rows.length > 0
            ? [h("div", { class: "tip-sub" }, `Of the ${input.limitName} limit used`), h("div", { class: "tip-rows" }, rows)]
            : h("div", { class: "tip-sub" }, "No readings yet"),
        ],
        marks,
      };
    };
  };
}

// ---- API cost bars ----------------------------------------------------------------------

export interface CostBucket {
  /** Epoch ms; the last bucket ends now, part way through. */
  start: number;
  end: number;
  usd: Map<number, number>;
  total: number;
}

/**
 * The cost chart's buckets: over 24 hours the last 24 hours on the clock (the current one so far
 * included), over 7 days the last 7 local days (today so far included). Each hour's cost goes to
 * the bucket its hour starts in.
 */
export function costBuckets(costs: HourlyCost[], rangeDays: RangeDays, nowMs: number): CostBucket[] {
  const starts: number[] = [];
  if (rangeDays === 1) {
    const hour = Math.floor(nowMs / HOUR_MS) * HOUR_MS;
    for (let i = 23; i >= 0; i--) starts.push(hour - i * HOUR_MS);
  } else {
    for (let i = -6; i <= 0; i++) starts.push(dayStart(nowMs, i));
  }
  const buckets = starts.map((start, i) => ({
    start,
    end: i + 1 < starts.length ? starts[i + 1] : nowMs,
    usd: new Map<number, number>(),
    total: 0,
  }));
  for (const c of costs) {
    const b = buckets.find((b, i) => c.ts >= b.start && (i + 1 === buckets.length || c.ts < b.end));
    if (!b) continue;
    b.usd.set(c.account_id, (b.usd.get(c.account_id) ?? 0) + c.usd);
    b.total += c.usd;
  }
  return buckets;
}

/** "$12.40", "$184.05", "$1,204", "<$0.01". */
export function usd(v: number): string {
  if (v > 0 && v < 0.005) return "<$0.01";
  const digits = v >= 1000 ? 0 : 2;
  return `$${v.toLocaleString("en-US", { minimumFractionDigits: digits, maximumFractionDigits: digits })}`;
}

/**
 * The cost axis: a round step that splits 0..max into about four parts (up to at least 4 cents, so
 * the labels always differ), the top, and how its labels read ("$5", "$0.25", "$0.025").
 */
export function costAxis(max: number): { step: number; top: number; label: (v: number) => string } {
  const span = Math.max(max, 0.04);
  const mag = 10 ** Math.floor(Math.log10(span / 4));
  const step = ([1, 2, 2.5, 5, 10].find((m) => m * mag >= span / 4 - 1e-12) ?? 10) * mag;
  const top = Math.ceil(span / step - 1e-9) * step;
  // Whole dollars, else cents, else as many decimals as the step needs.
  let decimals = 0;
  while (decimals < 4 && Math.abs(Math.round(step * 10 ** decimals) - step * 10 ** decimals) > 1e-6) decimals++;
  if (decimals > 0) decimals = Math.max(2, decimals);
  return {
    step,
    top,
    label: (v) => `$${v.toLocaleString("en-US", { minimumFractionDigits: decimals, maximumFractionDigits: decimals })}`,
  };
}

export interface CostInput {
  accounts: Account[];
  buckets: CostBucket[];
  color: (id: number) => string;
  rangeDays: RangeDays;
  nowMs: number;
}

/** A bar segment: square, or with its top corners rounded when it ends the stack. */
function segment(x: number, top: number, w: number, hgt: number, rounded: boolean): string {
  const r = rounded ? Math.min(4, hgt, w / 2) : 0;
  const bottom = top + hgt;
  return `M${x},${bottom}V${top + r}Q${x},${top} ${x + r},${top}H${x + w - r}Q${x + w},${top} ${x + w},${top + r}V${bottom}Z`;
}

/** "3 PM - 4 PM", "Fri 11 PM - 12 AM", "6 PM - now"; "Today", "Mon, Oct 20". */
function bucketName(b: CostBucket, rangeDays: RangeDays, nowMs: number): string {
  const start = new Date(b.start);
  const current = b.end === nowMs;
  if (rangeDays === 7) {
    if (current) return "Today so far";
    return start.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" });
  }
  const day = start.toDateString() === new Date(nowMs).toDateString() ? "" : `${weekday(start)} `;
  return `${day}${hourLabel(start)} - ${current ? "now" : hourLabel(new Date(b.end))}`;
}

export function drawCost(input: CostInput): Draw {
  return (f) => {
    const { svg, plot } = f;
    const { buckets, rangeDays } = input;
    const { step, top, label } = costAxis(Math.max(...buckets.map((b) => b.total)));
    const y = (v: number) => plot.top + (1 - v / top) * plot.height;
    const right = plot.left + plot.width;
    const bottom = plot.top + plot.height;

    for (let k = 0; k * step <= top + step / 2; k++) {
      const gy = Math.round(y(k * step)) + 0.5;
      svg.append(
        s("line", { class: k === 0 ? "axis" : "grid", x1: plot.left, x2: right, y1: gy, y2: gy }),
        text(plot.left - 8, gy + 4, label(k * step), "tick", "end"),
      );
    }

    const slot = plot.width / buckets.length;
    const width = Math.round(Math.min(rangeDays === 1 ? 22 : 40, slot * 0.66));
    const groups = buckets.map((b, i) => {
      const bx = Math.round(plot.left + slot * (i + 0.5) - width / 2);
      const g = s("g", { class: "bar" });
      let base = bottom;
      const stack = input.accounts.filter((a) => (b.usd.get(a.id) ?? 0) > 0);
      stack.forEach((a, k) => {
        const last = k === stack.length - 1;
        const full = (b.usd.get(a.id)! / top) * plot.height;
        // A 2px gap in the panel's color between segments; never less than a sliver.
        const hgt = Math.max(1, full - (last ? 0 : 2));
        g.append(s("path", { d: segment(bx, base - hgt, width, hgt, last), fill: input.color(a.id) }));
        base -= full;
      });
      svg.append(g);
      // Name the bars along the bottom: every third hour (the day's name at midnight), or every day.
      const start = new Date(b.start);
      const hour = start.getHours();
      if (rangeDays === 7 || hour % 3 === 0) {
        const label = rangeDays === 7 ? (i === buckets.length - 1 ? "Today" : weekday(start)) : hour === 0 ? weekday(start) : hourLabel(start);
        const strong = rangeDays === 7 ? i === buckets.length - 1 : hour === 0;
        svg.append(text(bx + width / 2, bottom + 17, label, strong ? "tick strong" : "tick"));
      }
      return g;
    });

    return (px) => {
      const i = px === null ? -1 : Math.floor((px - plot.left) / slot);
      groups.forEach((g, k) => g.classList.toggle("dim", i >= 0 && i < groups.length && k !== i));
      const b = buckets[i];
      if (!b) return null;
      const rows = input.accounts
        .filter((a) => (b.usd.get(a.id) ?? 0) > 0)
        .map((a) => [
          h("i", { class: "tip-swatch", style: { background: input.color(a.id) } }),
          h("b", null, usd(b.usd.get(a.id)!)),
          h("span", null, a.label),
        ]);
      return {
        crosshair: null,
        x: plot.left + slot * (i + 0.5) + width / 2,
        tip: [
          h("div", { class: "tip-head" }, bucketName(b, rangeDays, input.nowMs)),
          rows.length === 0
            ? h("div", { class: "tip-sub" }, "Nothing counted")
            : h(
                "div",
                { class: "tip-rows" },
                rows,
                rows.length > 1 && [h("div", { class: "tip-divider" }), h("i"), h("b", null, usd(b.total)), h("span", null, "in all")],
              ),
        ],
      };
    };
  };
}
