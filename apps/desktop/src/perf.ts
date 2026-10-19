// The performance overlay (D-128): a readout in the window's top-right
// corner, for working on the app itself. Off by default; Settings ›
// Developer turns it on, remembered on this Mac. While it's off nothing is
// measured: no frame loop, no timer.

const KEY = "otter.perf-overlay";

let on = read();
const listeners = new Set<(on: boolean) => void>();

function read(): boolean {
  try {
    return localStorage.getItem(KEY) === "on";
  } catch {
    return false;
  }
}

export function perfOverlay(): boolean {
  return on;
}

export function setPerfOverlay(next: boolean) {
  if (next === on) return;
  on = next;
  try {
    if (next) localStorage.setItem(KEY, "on");
    else localStorage.removeItem(KEY);
  } catch {
    // Not remembered; fine.
  }
  for (const f of listeners) f(next);
}

export function onPerfOverlay(f: (on: boolean) => void): () => void {
  listeners.add(f);
  return () => listeners.delete(f);
}

let terminalBytes = 0;

/** Bytes a terminal was sent (`Terminal`), counted whether or not the overlay is on: one add. */
export function countTerminalBytes(n: number) {
  terminalBytes += n;
}

export function terminalTotal(): number {
  return terminalBytes;
}

/** A second of measurements, as the overlay shows them. */
export interface Sample {
  fps: number;
  /** The longest gap between two frames, ms. */
  worstFrame: number;
  /** How late the main thread ran a 100 ms timer, at most, ms. */
  lag: number;
  domNodes: number;
  /** What the terminals were sent, bytes per second. */
  terminal: number;
  /** The JS heap in use, bytes, where the engine says (WebKit doesn't). */
  heap?: number;
}

/**
 * Counts frames and lateness, and makes a `Sample` of each second. A second
 * is counted from a frame: the first one only starts it, and each after it
 * ends a frame's time.
 */
export class Meter {
  private frames = 0;
  private worst = 0;
  private lag = 0;
  private last: number | undefined;

  constructor(
    private start: number,
    private bytes: number,
  ) {}

  frame(now: number) {
    if (this.last === undefined) this.start = now;
    else {
      this.worst = Math.max(this.worst, now - this.last);
      this.frames++;
    }
    this.last = now;
  }

  late(ms: number) {
    this.lag = Math.max(this.lag, ms);
  }

  /** Whether a second has passed since the first frame, so a sample is due. */
  due(now: number): boolean {
    return this.last !== undefined && now - this.start >= 1000;
  }

  /** Once a second has passed: its sample, and the next second starts. */
  sample(now: number, bytes: number, domNodes: number, heap?: number): Sample | null {
    if (!this.due(now)) return null;
    const span = now - this.start;
    const s: Sample = {
      fps: Math.round((this.frames * 1000) / span),
      worstFrame: Math.round(this.worst),
      lag: Math.round(this.lag),
      domNodes,
      terminal: Math.round(((bytes - this.bytes) * 1000) / span),
      ...(heap !== undefined ? { heap } : {}),
    };
    this.next(now, bytes);
    return s;
  }

  /** The window is back from hidden, which has no frames: start over, not count the gap. */
  resume(now: number, bytes: number) {
    this.last = undefined;
    this.next(now, bytes);
  }

  private next(now: number, bytes: number) {
    this.frames = 0;
    this.worst = 0;
    this.lag = 0;
    this.start = now;
    this.bytes = bytes;
  }
}

/** 1536 → "1.5 KB", for rates and sizes. */
export function bytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(n < 100 * 1024 * 1024 ? 1 : 0)} MB`;
}

/** Green from 55 fps, red under 30. */
export function fpsLevel(fps: number): "good" | "fair" | "poor" {
  return fps >= 55 ? "good" : fps >= 30 ? "fair" : "poor";
}
