import { useState, type KeyboardEvent, type PointerEvent } from "react";

/**
 * A panel beside the work whose width the developer sets (D-097): the list on
 * the left (workspaces, tasks), the Files/Timeline panel on the right.
 * `min` and `rest` match the panel's `min-width` and `max-width` in App.css.
 */
export type Panel = {
  /** Where its width is remembered on this Mac. */
  key: string;
  initial: number;
  min: number;
  /** Room always left for what's beside it. */
  rest: number;
  /** The edge its handle is on: dragging away from the panel widens it. */
  edge: "left" | "right";
};

export const SIDEBAR: Panel = { key: "otter.sidebar-width", initial: 280, min: 180, rest: 400, edge: "right" };
export const SIDE_PANEL: Panel = { key: "otter.side-panel-width", initial: 300, min: 220, rest: 320, edge: "left" };
/** The session on the right of a split (D-110); the left keeps 240 px. */
export const SPLIT: Panel = { key: "otter.split-width", initial: 560, min: 240, rest: 240, edge: "left" };
const STEP = 16;

/** At least the panel's least, and leaving `rest` of `room` (0: not known yet). */
export function clampWidth(p: Panel, w: number, room: number): number {
  const max = room > 0 ? Math.max(p.min, room - p.rest) : Infinity;
  return Math.min(max, Math.max(p.min, Math.round(w)));
}

function stored(p: Panel): number {
  try {
    const v = Number(localStorage.getItem(p.key));
    return v > 0 ? clampWidth(p, v, 0) : p.initial;
  } catch {
    return p.initial;
  }
}

function save(p: Panel, w: number) {
  try {
    localStorage.setItem(p.key, String(w));
  } catch {
    // Not remembered; fine.
  }
}

/** The width, and a setter that clamps (to the room it was given) and remembers it. */
export function usePanelWidth(p: Panel): [number, (w: number, room?: number) => void] {
  const [width, setWidth] = useState(() => stored(p));
  return [
    width,
    (w, room = 0) => {
      const next = clampWidth(p, w, room);
      setWidth(next);
      save(p, next);
    },
  ];
}

/**
 * The handle on a panel's inner edge: drag it, use ←/→ on it, or double-click
 * it for the panel's usual width. It sits between the panel and its neighbour
 * in their row, which is the room the panel shares.
 */
export function PanelGrip({
  panel,
  width,
  onResize,
  label,
}: {
  panel: Panel;
  width: number;
  onResize: (w: number, room?: number) => void;
  label: string;
}) {
  const sign = panel.edge === "right" ? 1 : -1;
  const room = (el: HTMLElement) => el.parentElement?.clientWidth ?? 0;

  function start(e: PointerEvent<HTMLDivElement>) {
    if (e.button !== 0) return;
    e.preventDefault();
    const grip = e.currentTarget;
    const from = { x: e.clientX, w: width, room: room(grip) };
    grip.setPointerCapture(e.pointerId);
    const move = (ev: globalThis.PointerEvent) => onResize(from.w + sign * (ev.clientX - from.x), from.room);
    const end = () => {
      grip.removeEventListener("pointermove", move);
      grip.removeEventListener("pointerup", end);
      grip.removeEventListener("pointercancel", end);
      grip.classList.remove("dragging");
      document.body.classList.remove("resizing-col");
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", end);
    grip.addEventListener("pointercancel", end);
    grip.classList.add("dragging");
    document.body.classList.add("resizing-col");
  }

  function key(e: KeyboardEvent<HTMLDivElement>) {
    const step = e.key === "ArrowRight" ? STEP : e.key === "ArrowLeft" ? -STEP : 0;
    if (!step) return;
    e.preventDefault();
    onResize(width + sign * step, room(e.currentTarget));
  }

  return (
    <div
      className="panel-grip"
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuenow={width}
      aria-valuemin={panel.min}
      tabIndex={0}
      title="Drag to resize · double-click to reset"
      onPointerDown={start}
      onKeyDown={key}
      onDoubleClick={() => onResize(panel.initial)}
    />
  );
}
