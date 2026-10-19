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
    const meter = new Meter(0, 1000);
    for (let t = 16; t <= 480; t += 16) meter.frame(t);
    meter.frame(520); // a 40 ms frame
    for (let t = 536; t < 1000; t += 16) meter.frame(t);
    meter.late(3);
    meter.late(12);
    meter.late(5);
    expect(meter.sample(992, 1000, 10)).toBeNull();
    meter.frame(1008);
    const s = meter.sample(1008, 1000 + 4096, 1234)!;
    // 61 frames in 1008 ms.
    expect(s.fps).toBe(61);
    expect(s.worstFrame).toBe(40);
    expect(s.lag).toBe(12);
    expect(s.domNodes).toBe(1234);
    expect(s.terminal).toBe(Math.round((4096 * 1000) / 1008));
    expect(s).not.toHaveProperty("heap");

    // The next second starts from nothing.
    for (let t = 1024; t <= 2008; t += 16) meter.frame(t);
    const t = meter.sample(2008, 1000 + 4096, 1234, 50 * 1024 * 1024)!;
    expect(t.worstFrame).toBe(16);
    expect(t.lag).toBe(0);
    expect(t.terminal).toBe(0);
    expect(t.heap).toBe(50 * 1024 * 1024);
  });

  it("doesn't count the time a hidden window drew nothing", () => {
    const meter = new Meter(0, 0);
    meter.frame(16);
    meter.resume(60_000, 0);
    for (let t = 60_016; t <= 61_000; t += 16) meter.frame(t);
    expect(meter.sample(61_000, 0, 1)!.worstFrame).toBe(16);
  });

  it("says sizes and rates briefly, and colours the frame rate", () => {
    expect(bytes(512)).toBe("512 B");
    expect(bytes(1536)).toBe("1.5 KB");
    expect(bytes(85 * 1024 * 1024)).toBe("85.0 MB");
    expect(bytes(1200 * 1024 * 1024)).toBe("1200 MB");
    expect([fpsLevel(60), fpsLevel(45), fpsLevel(12)]).toEqual(["good", "fair", "poor"]);
  });
});
