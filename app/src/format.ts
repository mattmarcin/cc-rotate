// Time and number formatting for the dashboard. Copy is sentence case, no em dashes.

const nowS = () => Date.now() / 1000;

/** "2h 14m", "38m", "3d 4h" until an epoch-seconds time; "now" once it has passed. */
export function until(atS: number): string {
  const secs = Math.round(atS - nowS());
  if (secs <= 0) return "now";
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${Math.max(m, 1)}m`;
}

/** "4:10 PM" today, "Thu 9:00 AM" this week, "Oct 30" further out. */
export function clock(atMs: number): string {
  const at = new Date(atMs);
  const now = new Date();
  const time = at.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (at.toDateString() === now.toDateString()) return time;
  const days = Math.abs(at.getTime() - now.getTime()) / 86_400_000;
  if (days < 6) return `${at.toLocaleDateString([], { weekday: "short" })} ${time}`;
  return at.toLocaleDateString([], { month: "short", day: "numeric" });
}

export function ago(tsMs: number): string {
  const secs = Math.round((Date.now() - tsMs) / 1000);
  if (secs < 45) return "just now";
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h ago`;
  return `${Math.round(secs / 86400)}d ago`;
}

export const pct = (n: number) => `${Math.round(n)}%`;

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}
