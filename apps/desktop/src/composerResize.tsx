import { useState, type KeyboardEvent, type PointerEvent } from "react";

/** The message box's height, remembered on this Mac. */
const KEY = "otter.composer-height";
export const DEFAULT_HEIGHT = 96;
const MIN_HEIGHT = 56;
/** Never more than this share of the window: the conversation stays visible. */
const MAX_SHARE = 0.6;
const STEP = 16;

export function clampHeight(h: number, windowHeight: number): number {
  const max = Math.max(MIN_HEIGHT, Math.round(windowHeight * MAX_SHARE));
  return Math.min(max, Math.max(MIN_HEIGHT, Math.round(h)));
}

function stored(): number {
  try {
    const v = Number(localStorage.getItem(KEY));
    return v > 0 ? clampHeight(v, window.innerHeight) : DEFAULT_HEIGHT;
  } catch {
    return DEFAULT_HEIGHT;
  }
}

function save(h: number) {
  try {
    localStorage.setItem(KEY, String(h));
  } catch {
    // Not remembered; fine.
  }
}

/** The height, and a setter that clamps and remembers it. */
export function useComposerHeight(): [number, (h: number) => void] {
  const [height, setHeight] = useState(stored);
  return [
    height,
    (h) => {
      const next = clampHeight(h, window.innerHeight);
      setHeight(next);
      save(next);
    },
  ];
}

/**
 * The handle on the message box's top edge: drag it up for a taller box,
 * use ↑/↓ on it, or double-click it for the default height.
 */
export function ComposerGrip({ height, onResize }: { height: number; onResize: (h: number) => void }) {
  function start(e: PointerEvent<HTMLDivElement>) {
    if (e.button !== 0) return;
    e.preventDefault();
    const grip = e.currentTarget;
    const from = { y: e.clientY, h: height };
    grip.setPointerCapture(e.pointerId);
    const move = (ev: globalThis.PointerEvent) => onResize(from.h + (from.y - ev.clientY));
    const end = () => {
      grip.removeEventListener("pointermove", move);
      grip.removeEventListener("pointerup", end);
      grip.removeEventListener("pointercancel", end);
      document.body.classList.remove("resizing-row");
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", end);
    grip.addEventListener("pointercancel", end);
    document.body.classList.add("resizing-row");
  }

  function key(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === "ArrowUp" || e.key === "ArrowDown") {
      e.preventDefault();
      onResize(height + (e.key === "ArrowUp" ? STEP : -STEP));
    }
  }

  return (
    <div
      className="composer-grip"
      role="separator"
      aria-orientation="horizontal"
      aria-label="Message box height"
      aria-valuenow={height}
      tabIndex={0}
      title="Drag to resize · double-click to reset"
      onPointerDown={start}
      onKeyDown={key}
      onDoubleClick={() => onResize(DEFAULT_HEIGHT)}
    />
  );
}
