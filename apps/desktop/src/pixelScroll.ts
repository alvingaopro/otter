// Pixel scrolling (D-130). xterm.js 6 draws whole rows, so a session's
// history moved a row (21 px) at a time, and a slow trackpad scroll changed
// the picture 8 to 20 times a second. Its scroll position is in pixels,
// though: the screen is moved by what's left over past the row drawn, so the
// text follows the finger, and settles on a row once the scrolling stops.
// xterm.js #6039 does the same inside xterm; drop this once it's merged.

interface ScrollPosition {
  scrollTop: number;
  /** False when only the history's length changed (output), not the position. */
  scrollTopChanged?: boolean;
}

/** xterm's private parts this needs; if a later xterm lacks one, nothing happens. */
interface Core {
  _viewport?: {
    _scrollableElement?: {
      onScroll?(listener: (e: ScrollPosition) => void): { dispose(): void };
      getScrollPosition?(): ScrollPosition;
    };
    scrollToLine?(line: number, disableSmoothScroll?: boolean): void;
  };
  _renderService?: { dimensions?: { css?: { cell?: { height?: number } } } };
}

/** What's needed of the terminal. */
export interface Scrolled {
  readonly element: HTMLElement | undefined;
  readonly buffer: {
    readonly active: { readonly viewportY: number };
    onBufferChange(listener: () => void): { dispose(): void };
  };
  onResize(listener: () => void): { dispose(): void };
}

/** How long scrolling stops before the screen settles on a row. */
export const SETTLE_MS = 150;

/**
 * Moves `term`'s screen by the part of a row its scroll position is past the
 * row drawn, and settles it on a row when scrolling stops. Returns what undoes
 * it.
 */
export function pixelScroll(term: Scrolled): () => void {
  const core = (term as { _core?: Core })._core;
  const viewport = core?._viewport;
  const scrollable = viewport?._scrollableElement;
  const screen = term.element?.querySelector<HTMLElement>(".xterm-screen");
  if (!viewport?.scrollToLine || !scrollable?.onScroll || !scrollable.getScrollPosition || !screen) {
    return () => {};
  }
  const position = () => scrollable.getScrollPosition!().scrollTop;
  /** A scroll this makes, onto a row; not one to settle after. */
  let ours = false;
  const toLine = (line: number) => {
    ours = true;
    try {
      viewport.scrollToLine!(line, true);
    } finally {
      ours = false;
    }
  };

  let frame = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  /** The next placement settles on a row: eased, not following a finger. */
  let settling = false;

  const place = (offset: number, eased: boolean) => {
    // Whole device pixels: a canvas moved by part of one is redrawn blurred.
    const dpr = window.devicePixelRatio || 1;
    const px = Math.round(offset * dpr) / dpr;
    screen.style.transition = eased ? "transform 80ms ease-out" : "";
    screen.style.transform = px ? `translateY(${px}px)` : "";
  };
  // After xterm has drawn the row: its frame was asked for first, in its own
  // scroll handler, so both reach the screen together.
  const apply = () => {
    frame = 0;
    const eased = settling;
    settling = false;
    // Read each time: ⌘= / ⌘- and a font loading change it.
    const cell = core?._renderService?.dimensions?.css?.cell?.height ?? 0;
    const line = term.buffer.active.viewportY;
    const offset = line * cell - position();
    if (!cell || Math.abs(offset) > cell / 2 + 1) {
      // The position and the row drawn disagree (the cells changed size, or
      // xterm moved the row itself): take the row.
      place(0, false);
      if (cell) toLine(line);
      return;
    }
    place(offset, eased);
  };
  const schedule = () => {
    if (!frame) frame = requestAnimationFrame(apply);
  };
  const settle = () => {
    timer = undefined;
    settling = true;
    toLine(term.buffer.active.viewportY);
    schedule();
  };
  const scrolled = (e: ScrollPosition) => {
    // Only the position moving is scrolling. Output lengthening the history
    // isn't, and would put off settling for as long as it flows.
    if (!ours && e.scrollTopChanged !== false) {
      // A scroll right as it settles follows the finger, not eased.
      settling = false;
      clearTimeout(timer);
      timer = setTimeout(settle, SETTLE_MS);
    }
    schedule();
  };

  screen.style.willChange = "transform";
  const subscriptions = [
    scrollable.onScroll(scrolled),
    // Another screen (a full-screen program) has no history to be part way into.
    term.buffer.onBufferChange(() => {
      clearTimeout(timer);
      place(0, false);
    }),
    term.onResize(schedule),
  ];
  return () => {
    clearTimeout(timer);
    cancelAnimationFrame(frame);
    for (const s of subscriptions) s.dispose();
    screen.style.transform = "";
    screen.style.transition = "";
    screen.style.willChange = "";
  };
}
