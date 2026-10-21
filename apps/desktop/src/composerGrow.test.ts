import { describe, expect, it } from "vitest";
import { atEnd, fitHeight, MAX_SHARE } from "./composerGrow";

describe("the message box growing (D-177)", () => {
  it("is as tall as what is written, up to its share of the window, then scrolls", () => {
    expect(fitHeight(34, 1000)).toEqual({ height: 34, scrolls: false });
    expect(fitHeight(600, 1000)).toEqual({ height: 600, scrolls: false });
    expect(fitHeight(900, 1000)).toEqual({ height: 1000 * MAX_SHARE, scrolls: true });
  });

  it("counts the conversation at its end within a few pixels of it", () => {
    const el = (scrollHeight: number, scrollTop: number, clientHeight: number) =>
      ({ scrollHeight, scrollTop, clientHeight }) as HTMLElement;
    expect(atEnd(el(1000, 600, 400))).toBe(true);
    expect(atEnd(el(1000, 590, 400))).toBe(true);
    expect(atEnd(el(1000, 300, 400))).toBe(false);
  });
});
