// Usage over time: one stepped line per account, with swaps drawn as vertical markers.
import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import type { Account, EventRow, Sample } from "./api";

export type Metric = "weekly" | "five_hour";

const LIGHT = ["#0F7C86", "#4C5BD4", "#9B3F8F", "#6E7F1F", "#1F7FBF", "#A65B2A", "#5A6B8C", "#C2457A"];
const DARK = ["#3FB6BF", "#8C97F2", "#D07CC4", "#A9BD52", "#5CB2EE", "#E09A63", "#93A3C2", "#EB86B0"];

const dark = () => window.matchMedia("(prefers-color-scheme: dark)").matches;

export function accountColor(index: number): string {
  const palette = dark() ? DARK : LIGHT;
  return palette[index % palette.length];
}

function css(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/** Points for one account, with a drop to 0 inserted wherever a window reset between samples. */
function points(samples: Sample[], metric: Metric, nowS: number): [number, number][] {
  const out: [number, number][] = [];
  const value = (s: Sample) => (metric === "weekly" ? s.week_pct : s.five_pct);
  const resets = (s: Sample) => (metric === "weekly" ? s.week_resets_at : s.five_resets_at);
  samples.forEach((s, i) => {
    const v = value(s);
    if (v === null) return;
    out.push([Math.floor(s.ts / 1000), v]);
    const r = resets(s);
    const nextTs = i + 1 < samples.length ? samples[i + 1].ts / 1000 : nowS;
    if (r !== null && r < nextTs && r > s.ts / 1000) out.push([r, 0]);
  });
  return out;
}

export interface ChartInput {
  accounts: Account[];
  samples: Sample[];
  swaps: EventRow[];
  metric: Metric;
  sinceMs: number;
  threshold: number;
}

export class UsageChart {
  private plot: uPlot | null = null;
  private observer: ResizeObserver;

  constructor(private host: HTMLElement) {
    this.observer = new ResizeObserver(() => this.resize());
    this.observer.observe(host);
  }

  private resize() {
    if (this.plot) this.plot.setSize({ width: this.host.clientWidth, height: this.host.clientHeight });
  }

  render(input: ChartInput) {
    const nowS = Date.now() / 1000;
    const sinceS = input.sinceMs / 1000;
    const byAccount = input.accounts.map((a) =>
      points(
        input.samples.filter((s) => s.account_id === a.id),
        input.metric,
        nowS,
      ),
    );
    const xs = Array.from(new Set(byAccount.flat().map(([t]) => t).concat([sinceS, nowS]))).sort((a, b) => a - b);
    const index = new Map(xs.map((t, i) => [t, i]));
    const series = byAccount.map((pts) => {
      const ys: (number | null)[] = new Array(xs.length).fill(null);
      for (const [t, v] of pts) ys[index.get(t)!] = v;
      return ys;
    });

    const ink = css("--muted");
    const grid = css("--line");
    const swaps = input.swaps.map((e) => ({
      x: e.ts / 1000,
      to: input.accounts.findIndex((a) => a.id === e.account_to),
    }));
    const threshold = input.threshold;

    const opts: uPlot.Options = {
      width: this.host.clientWidth,
      height: this.host.clientHeight,
      legend: { show: false },
      cursor: { drag: { x: false, y: false }, points: { size: 6 } },
      padding: [12, 8, 0, 0],
      scales: { x: { time: true, min: sinceS, max: nowS }, y: { range: [0, 100] } },
      axes: [
        {
          stroke: ink,
          grid: { show: false },
          ticks: { stroke: grid },
          space: 72,
          font: "12px 'Schibsted Grotesk Variable', system-ui",
        },
        {
          stroke: ink,
          grid: { stroke: grid, width: 1 },
          ticks: { show: false },
          size: 44,
          font: "12px 'Schibsted Grotesk Variable', system-ui",
          values: (_u, ticks) => ticks.map((t) => `${t}%`),
          splits: () => [0, 25, 50, 75, 100],
        },
      ],
      series: [
        {},
        ...input.accounts.map((a, i) => ({
          label: a.label,
          stroke: accountColor(i),
          width: 2,
          spanGaps: true,
          points: { show: false },
          paths: uPlot.paths.stepped!({ align: 1 }),
        })),
      ],
      hooks: {
        draw: [
          (u) => {
            const ctx = u.ctx;
            ctx.save();
            const y = u.valToPos(threshold, "y", true);
            ctx.strokeStyle = css("--amber");
            ctx.globalAlpha = 0.7;
            ctx.setLineDash([2, 4]);
            ctx.lineWidth = 1 * devicePixelRatio;
            ctx.beginPath();
            ctx.moveTo(u.bbox.left, y);
            ctx.lineTo(u.bbox.left + u.bbox.width, y);
            ctx.stroke();
            ctx.globalAlpha = 0.5;
            ctx.setLineDash([4, 3]);
            for (const s of swaps) {
              if (s.x < sinceS) continue;
              const x = u.valToPos(s.x, "x", true);
              ctx.strokeStyle = s.to >= 0 ? accountColor(s.to) : ink;
              ctx.beginPath();
              ctx.moveTo(x, u.bbox.top);
              ctx.lineTo(x, u.bbox.top + u.bbox.height);
              ctx.stroke();
            }
            ctx.restore();
          },
        ],
      },
    };

    this.plot?.destroy();
    this.plot = new uPlot(opts, [xs, ...series] as uPlot.AlignedData, this.host);
  }

  destroy() {
    this.observer.disconnect();
    this.plot?.destroy();
  }
}
