// Numbers for people: bytes, rates, percentages, durations.

const UNITS = ["B", "KB", "MB", "GB", "TB"];

/** 1,536 → "1.5 KB" (base 1024, at most one decimal). */
export function bytes(n: number): string {
  let i = 0;
  let v = n;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${UNITS[i]}`;
}

export const rate = (bps: number) => `${bytes(bps)}/s`;

export const percent = (v: number) => `${v >= 10 || v === 0 ? Math.round(v) : v.toFixed(1)}%`;

/** 93,784 s → "1d 2h". */
export function uptime(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

/** Seconds before now, for chart readouts: "now", "45s ago", "3m 20s ago". */
export function before(secs: number): string {
  if (secs < 3) return "now";
  const m = Math.floor(secs / 60);
  const s = Math.round(secs % 60);
  return m > 0 ? `${m}m ${s}s ago` : `${s}s ago`;
}
