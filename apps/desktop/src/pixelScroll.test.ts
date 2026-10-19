import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { pixelScroll, SETTLE_MS } from "./pixelScroll";

/** A terminal with xterm's scroll position and the row it draws, as xterm keeps them. */
function terminal() {
  const cell = { height: 21 };
  let scrollTop = 0;
  let scrolled: ((e: { scrollTop: number; scrollTopChanged: boolean }) => void)[] = [];
  let bufferChanged: (() => void)[] = [];
  const element = document.createElement("div");
  const screen = document.createElement("div");
  screen.className = "xterm-screen";
  element.appendChild(screen);
  const fire = (scrollTopChanged = true) => scrolled.forEach((l) => l({ scrollTop, scrollTopChanged }));
  const buffer = {
    active: { viewportY: 0 },
    onBufferChange: (l: () => void) => {
      bufferChanged.push(l);
      return { dispose: () => (bufferChanged = bufferChanged.filter((x) => x !== l)) };
    },
  };
  const scrollToLine = vi.fn((line: number) => {
    scrollTop = line * cell.height;
    buffer.active.viewportY = line;
    fire();
  });
  const term = {
    element,
    buffer,
    onResize: () => ({ dispose: () => {} }),
    _core: {
      _viewport: {
        _scrollableElement: {
          onScroll: (l: (e: { scrollTop: number; scrollTopChanged: boolean }) => void) => {
            scrolled.push(l);
            return { dispose: () => (scrolled = scrolled.filter((x) => x !== l)) };
          },
          getScrollPosition: () => ({ scrollTop }),
        },
        scrollToLine,
      },
      _renderService: { dimensions: { css: { cell } } },
    },
  };
  return {
    term,
    screen,
    cell,
    scrollToLine,
    /** A wheel moving the scroll position, as xterm's viewport does: the row drawn is the nearest. */
    scrollTo(top: number) {
      scrollTop = top;
      buffer.active.viewportY = Math.round(top / cell.height);
      fire();
    },
    otherScreen: () => bufferChanged.forEach((l) => l()),
    /** Output lengthening the history: xterm's viewport tells of it, the position unchanged. */
    grow: () => fire(false),
  };
}
const frame = () => vi.advanceTimersByTime(17);
const ratio = (value: number) => Object.defineProperty(window, "devicePixelRatio", { value, configurable: true });

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "requestAnimationFrame", "cancelAnimationFrame"] });
  ratio(1);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("pixel scrolling (D-130)", () => {
  it("moves the screen by the part of a row the position is past the row drawn", () => {
    const t = terminal();
    pixelScroll(t.term);
    t.scrollTo(10 * 21 + 5);
    expect(t.screen.style.transform).toBe("");
    frame();
    expect(t.screen.style.transform).toBe("translateY(-5px)");
    t.scrollTo(10 * 21 - 4);
    frame();
    expect(t.screen.style.transform).toBe("translateY(4px)");
  });

  it("moves it by whole device pixels", () => {
    ratio(2);
    const t = terminal();
    pixelScroll(t.term);
    t.scrollTo(10 * 21 + 5.3);
    frame();
    expect(t.screen.style.transform).toBe("translateY(-5.5px)");
  });

  it("settles on the row drawn once scrolling stops, eased, and only once", () => {
    const t = terminal();
    pixelScroll(t.term);
    t.scrollTo(10 * 21 + 5);
    frame();
    vi.advanceTimersByTime(SETTLE_MS - 20);
    t.scrollTo(10 * 21 + 8);
    frame();
    expect(t.scrollToLine).not.toHaveBeenCalled();
    vi.advanceTimersByTime(SETTLE_MS);
    expect(t.scrollToLine).toHaveBeenCalledExactlyOnceWith(10, true);
    frame();
    expect(t.screen.style.transform).toBe("");
    expect(t.screen.style.transition).toContain("transform");
    vi.advanceTimersByTime(SETTLE_MS * 4);
    expect(t.scrollToLine).toHaveBeenCalledOnce();
  });

  it("settles on time while output lengthens the history", () => {
    const t = terminal();
    pixelScroll(t.term);
    t.scrollTo(10 * 21 + 5);
    frame();
    vi.advanceTimersByTime(SETTLE_MS - 50);
    t.grow();
    vi.advanceTimersByTime(60);
    expect(t.scrollToLine).toHaveBeenCalledExactlyOnceWith(10, true);
  });

  it("follows the finger, not eased, when a scroll comes right as it settles", () => {
    const t = terminal();
    pixelScroll(t.term);
    t.scrollTo(10 * 21 + 5);
    // Due with the settle's timer, so after it and before its frame.
    setTimeout(() => t.scrollTo(10 * 21 + 3), SETTLE_MS);
    vi.advanceTimersByTime(SETTLE_MS);
    expect(t.scrollToLine).toHaveBeenCalledOnce();
    frame();
    expect(t.screen.style.transition).toBe("");
    expect(t.screen.style.transform).toBe("translateY(-3px)");
  });

  it("follows the finger again at once after settling", () => {
    const t = terminal();
    pixelScroll(t.term);
    t.scrollTo(10 * 21 + 5);
    frame();
    vi.advanceTimersByTime(SETTLE_MS);
    frame();
    t.scrollTo(10 * 21 + 3);
    frame();
    expect(t.screen.style.transition).toBe("");
    expect(t.screen.style.transform).toBe("translateY(-3px)");
  });

  it("takes the row when the position and the row drawn disagree (the cells changed size)", () => {
    const t = terminal();
    pixelScroll(t.term);
    t.scrollTo(10 * 21 + 5);
    frame();
    t.cell.height = 30;
    t.scrollTo(10 * 21 + 6);
    // xterm still draws row 10, now 300 px down: 84 px off is no part of a row.
    t.term.buffer.active.viewportY = 10;
    frame();
    expect(t.screen.style.transform).toBe("");
    expect(t.scrollToLine).toHaveBeenCalledWith(10, true);
  });

  it("lets go when another screen (a full-screen program) is shown", () => {
    const t = terminal();
    pixelScroll(t.term);
    t.scrollTo(10 * 21 + 5);
    frame();
    t.otherScreen();
    expect(t.screen.style.transform).toBe("");
    vi.advanceTimersByTime(SETTLE_MS * 2);
    expect(t.scrollToLine).not.toHaveBeenCalled();
  });

  it("does nothing on an xterm without the parts it needs", () => {
    const t = terminal();
    const { _core: _, ...plain } = t.term;
    const undo = pixelScroll(plain);
    t.scrollTo(10 * 21 + 5);
    frame();
    expect(t.screen.style.transform).toBe("");
    undo();
  });

  it("undoes itself", () => {
    const t = terminal();
    const undo = pixelScroll(t.term);
    expect(t.screen.style.willChange).toBe("transform");
    t.scrollTo(10 * 21 + 5);
    frame();
    undo();
    expect(t.screen.style.transform).toBe("");
    expect(t.screen.style.willChange).toBe("");
    t.scrollTo(10 * 21 + 7);
    frame();
    vi.advanceTimersByTime(SETTLE_MS * 2);
    expect(t.screen.style.transform).toBe("");
    expect(t.scrollToLine).not.toHaveBeenCalled();
  });
});
