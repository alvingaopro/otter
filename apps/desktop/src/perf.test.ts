import { beforeEach, describe, expect, it, vi } from "vitest";
import { Meter, bytes, fpsLevel } from "./perf";

describe("the performance overlay's switch (D-128)", () => {
  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  it("is off until turned on, and then remembered on this Mac", async () => {
    const first = await import("./perf");
    expect(first.perfOverlay()).toBe(false);
    const seen: boolean[] = [];
    const stop = first.onPerfOverlay((on) => seen.push(on));
    first.setPerfOverlay(true);
    first.setPerfOverlay(true);
    stop();
    first.setPerfOverlay(false);
    expect(seen).toEqual([true]);

    first.setPerfOverlay(true);
    vi.resetModules();
    const next = await import("./perf");
    expect(next.perfOverlay()).toBe(true);
  });
});

describe("a second of measurements", () => {
  it("counts frames, the slowest one, lateness and the terminals' bytes", () => {
    const meter = new Meter(-500, 1000);
    // 50 Hz from 0 ms, with the frames at 500 and 520 dropped: a 60 ms frame.
    for (let t = 0; t <= 1000; t += 20) if (t !== 500 && t !== 520) meter.frame(t);
    meter.late(3);
    meter.late(12);
    meter.late(5);
    expect(meter.due(980)).toBe(false);
    expect(meter.sample(980, 1000, 10)).toBeNull();
    expect(meter.due(1000)).toBe(true);
    const s = meter.sample(1000, 1000 + 4096, 1234)!;
    // Counted from the first frame, not from when the meter was made.
    expect(s.fps).toBe(48);
    expect(s.worstFrame).toBe(60);
    expect(s.lag).toBe(12);
    expect(s.domNodes).toBe(1234);
    expect(s.terminal).toBe(4096);
    expect(s).not.toHaveProperty("heap");

    // The next second starts from nothing.
    for (let t = 1020; t <= 2000; t += 20) meter.frame(t);
    const t = meter.sample(2000, 1000 + 4096, 1234, 50 * 1024 * 1024)!;
    expect(t.fps).toBe(50);
    expect(t.worstFrame).toBe(20);
    expect(t.lag).toBe(0);
    expect(t.terminal).toBe(0);
    expect(t.heap).toBe(50 * 1024 * 1024);
  });

  it("doesn't count the time a hidden window drew nothing", () => {
    const meter = new Meter(0, 0);
    meter.frame(16);
    meter.frame(32);
    meter.resume(60_000, 0);
    expect(meter.due(61_000)).toBe(false);
    for (let t = 60_020; t <= 61_020; t += 20) meter.frame(t);
    const s = meter.sample(61_020, 0, 1)!;
    expect(s.worstFrame).toBe(20);
    expect(s.fps).toBe(50);
  });

  it("says sizes and rates briefly, and colours the frame rate", () => {
    expect(bytes(512)).toBe("512 B");
    expect(bytes(1536)).toBe("1.5 KB");
    expect(bytes(85 * 1024 * 1024)).toBe("85.0 MB");
    expect(bytes(1200 * 1024 * 1024)).toBe("1200 MB");
    expect([fpsLevel(60), fpsLevel(45), fpsLevel(12)]).toEqual(["good", "fair", "poor"]);
  });
});
