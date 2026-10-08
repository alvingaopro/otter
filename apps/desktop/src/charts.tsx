// Small SVG charts for the host page, to the house spec: 2px lines, a 10%
// area wash for a single series, hairline grid, one y-axis, a legend for two
// series, values in ink (never in the series color), and a crosshair with one
// tooltip listing every series.

import { useLayoutEffect, useRef, useState } from "react";
import { before } from "./format";

export interface Series {
  name: string;
  /** A CSS color (a token: var(--series-1)). */
  color: string;
  values: number[];
}

/** A clean axis maximum at or above `v`; in 1024-steps for byte values. */
function niceMax(v: number, binary: boolean): number {
  if (v <= 0) return binary ? 1024 : 1;
  if (binary) {
    const unit = 1024 ** Math.floor(Math.log(v) / Math.log(1024));
    return niceMax(v / unit, false) * unit;
  }
  const p = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * p >= v) return m * p;
  return 10 * p;
}

function useWidth(): [React.RefObject<HTMLDivElement | null>, number] {
  const ref = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(300);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = () => setW(Math.max(120, Math.floor(el.clientWidth)));
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [ref, w];
}

/** A line chart over the sampled history (oldest first, `interval` s apart). */
export function LineChart({
  title,
  series,
  interval,
  format,
  max,
  height = 132,
  binary = false,
  time,
}: {
  title: string;
  series: Series[];
  interval: number;
  format: (v: number) => string;
  /** Fixed y maximum (e.g. 100 for percent); else a clean step above the data. */
  max?: number;
  height?: number;
  /** Values are bytes: axis steps in powers of 1024. */
  binary?: boolean;
  /**
   * A time axis instead of evenly spaced samples: each value's time (ms) and
   * the span shown. Points further apart than `gap` ms aren't joined (the
   * host wasn't recording).
   */
  time?: { at: number[]; from: number; to: number; gap: number; label: (t: number) => string };
}) {
  const [ref, width] = useWidth();
  const [hover, setHover] = useState<number | null>(null);
  const n = Math.max(...series.map((s) => s.values.length), 0);
  const yMax = max ?? niceMax(Math.max(0, ...series.flatMap((s) => s.values)) * 1.1, binary);
  // Room for the widest tick label (11px mono ≈ 6.7px a character).
  const left = Math.max(...[0, yMax / 2, yMax].map((t) => format(t).length)) * 6.7 + 12;
  const right = 8;
  const top = 8;
  const bottom = 20;
  const plotW = width - left - right;
  const plotH = height - top - bottom;
  const x = time
    ? (i: number) => left + ((time.at[i] - time.from) / Math.max(1, time.to - time.from)) * plotW
    : (i: number) => left + (n <= 1 ? plotW : (i / (n - 1)) * plotW);
  const y = (v: number) => top + plotH - (Math.min(v, yMax) / yMax) * plotH;
  const ticks = [0, yMax / 2, yMax];
  // A new segment ("M") after a gap in recording.
  const path = (vals: number[]) =>
    vals
      .map((v, i) => {
        const jump = i === 0 || (time !== undefined && time.at[i] - time.at[i - 1] > time.gap);
        return `${jump ? "M" : "L"}${x(i).toFixed(1)},${y(v).toFixed(1)}`;
      })
      .join("");
  const single = series.length === 1;

  function onMove(e: React.PointerEvent<SVGRectElement>) {
    if (n === 0) return;
    const r = e.currentTarget.getBoundingClientRect();
    const fx = (e.clientX - r.left) / r.width;
    if (time) {
      const t = time.from + fx * (time.to - time.from);
      let best = 0;
      time.at.forEach((ti, i) => {
        if (Math.abs(ti - t) < Math.abs(time.at[best] - t)) best = i;
      });
      setHover(best);
    } else {
      setHover(Math.max(0, Math.min(n - 1, Math.round(fx * (n - 1)))));
    }
  }

  return (
    <figure className="chart">
      <figcaption className="chart-head">
        <span className="chart-title">{title}</span>
        {!single && (
          <span className="legend">
            {series.map((s) => (
              <span key={s.name} className="legend-item">
                <span className="legend-key" style={{ background: s.color }} />
                {s.name} <b>{format(s.values[s.values.length - 1] ?? 0)}</b>
              </span>
            ))}
          </span>
        )}
        {single && <b className="chart-now">{format(series[0].values[series[0].values.length - 1] ?? 0)}</b>}
      </figcaption>
      <div className="chart-box" ref={ref}>
        <svg width={width} height={height} role="img" aria-label={title}>
          {ticks.map((t) => (
            <g key={t}>
              <line x1={left} x2={width - right} y1={y(t)} y2={y(t)} className="grid" />
              <text x={left - 6} y={y(t) + 4} className="axis" textAnchor="end">
                {format(t)}
              </text>
            </g>
          ))}
          <text x={left} y={height - 4} className="axis">
            {time ? time.label(time.from) : before((n - 1) * interval)}
          </text>
          <text x={width - right} y={height - 4} className="axis" textAnchor="end">
            now
          </text>
          {single && n > 1 && !time && (
            <path
              d={`${path(series[0].values)}L${x(n - 1)},${y(0)}L${x(0)},${y(0)}Z`}
              fill={series[0].color}
              opacity={0.1}
            />
          )}
          {series.map((s) => (
            <path key={s.name} d={path(s.values)} fill="none" stroke={s.color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />
          ))}
          {hover !== null && (
            <g>
              <line x1={x(hover)} x2={x(hover)} y1={top} y2={top + plotH} className="crosshair" />
              {series.map((s) => (
                <circle key={s.name} cx={x(hover)} cy={y(s.values[hover] ?? 0)} r={4} fill={s.color} className="dot" />
              ))}
            </g>
          )}
          <rect
            x={left}
            y={top}
            width={plotW}
            height={plotH}
            fill="transparent"
            onPointerMove={onMove}
            onPointerLeave={() => setHover(null)}
          />
        </svg>
        {hover !== null && (
          <div
            className="tooltip"
            style={{ left: Math.min(x(hover) + 10, width - 150), top: 6 }}
            role="status"
          >
            <div className="tooltip-when">{time ? time.label(time.at[hover]) : before((n - 1 - hover) * interval)}</div>
            {series.map((s) => (
              <div key={s.name} className="tooltip-row">
                <span className="tooltip-key" style={{ background: s.color }} />
                <b>{format(s.values[hover] ?? 0)}</b>
                {!single && <span className="muted">{s.name}</span>}
              </div>
            ))}
          </div>
        )}
      </div>
    </figure>
  );
}

/** A trend line for a stat tile (no axes; the tile carries the value). */
export function Sparkline({ values, max, color = "var(--series-1)" }: { values: number[]; max?: number; color?: string }) {
  const w = 120;
  const h = 28;
  if (values.length < 2) return <svg width={w} height={h} aria-hidden="true" />;
  const m = max ?? Math.max(...values, 1);
  const pts = values.map((v, i) => `${((i / (values.length - 1)) * w).toFixed(1)},${(h - 2 - (Math.min(v, m) / m) * (h - 4)).toFixed(1)}`);
  return (
    <svg width={w} height={h} aria-hidden="true" className="sparkline">
      <polyline points={pts.join(" ")} fill="none" stroke={color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  );
}

/** How full something is: the fill carries severity, always with its label. */
export function Meter({ used, total, label }: { used: number; total: number; label: string }) {
  const frac = total > 0 ? Math.min(1, used / total) : 0;
  const level = frac >= 0.9 ? "critical" : frac >= 0.75 ? "warning" : "normal";
  return (
    <div className="meter" role="meter" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(frac * 100)} aria-label={label}>
      <div className={`meter-fill ${level}`} style={{ width: `${frac * 100}%` }} />
    </div>
  );
}
