import { describe, expect, it } from "vitest";
import { keepSessions, type Kept } from "./keepSessions";

const MIN = 60_000;
const all = new Set(["a", "b", "c", "d", "e", "f", "g"]);
const opts = (now: number, capacity = 5, idleMinutes = 10) => ({ now, capacity, idleMinutes });
const ids = (k: Kept[]) => k.map((x) => x.id);

describe("keepSessions", () => {
  it("keeps the ones shown", () => {
    const k = keepSessions([], ["a"], all, opts(0));
    expect(k).toEqual([{ id: "a", hiddenAt: null }]);
  });

  it("keeps one that was shown, hidden, and notes when it was hidden", () => {
    let k = keepSessions([], ["a"], all, opts(0));
    k = keepSessions(k, ["b"], all, opts(5));
    expect(k).toEqual([
      { id: "a", hiddenAt: 5 },
      { id: "b", hiddenAt: null },
    ]);
  });

  it("keeps their order, so the terminals aren't moved", () => {
    let k = keepSessions([], ["a"], all, opts(0));
    k = keepSessions(k, ["b"], all, opts(1));
    k = keepSessions(k, ["a"], all, opts(2));
    expect(ids(k)).toEqual(["a", "b"]);
  });

  it("lets the one hidden longest go when the capacity is full", () => {
    let k: Kept[] = [];
    // Shown a, b, c, d, e one after another, then f: capacity 5 keeps b..f.
    for (const [t, id] of ["a", "b", "c", "d", "e", "f"].entries()) k = keepSessions(k, [id], all, opts(t));
    expect(ids(k)).toEqual(["b", "c", "d", "e", "f"]);
  });

  it("counts a second one shown against the capacity", () => {
    let k: Kept[] = [];
    for (const [t, id] of ["a", "b", "c"].entries()) k = keepSessions(k, [id], all, opts(t, 3));
    k = keepSessions(k, ["d", "e"], all, opts(10, 3));
    expect(ids(k)).toEqual(["c", "d", "e"]);
    // Two shown fill a capacity of two: nothing else stays.
    k = keepSessions(k, ["d", "e"], all, opts(11, 2));
    expect(ids(k)).toEqual(["d", "e"]);
  });

  it("keeps what is shown whatever the capacity", () => {
    const k = keepSessions([], ["a", "b"], all, opts(0, 1));
    expect(ids(k)).toEqual(["a", "b"]);
  });

  it("detaches one that has been hidden for the idle time, not before", () => {
    let k = keepSessions([], ["a"], all, opts(0));
    k = keepSessions(k, ["b"], all, opts(1 * MIN));
    expect(ids(keepSessions(k, ["b"], all, opts(11 * MIN - 1)))).toEqual(["a", "b"]);
    expect(ids(keepSessions(k, ["b"], all, opts(11 * MIN)))).toEqual(["b"]);
  });

  it("starts the idle time again when it is shown and hidden again", () => {
    let k = keepSessions([], ["a"], all, opts(0));
    k = keepSessions(k, ["b"], all, opts(1 * MIN));
    k = keepSessions(k, ["a"], all, opts(8 * MIN));
    k = keepSessions(k, ["b"], all, opts(9 * MIN));
    expect(ids(keepSessions(k, ["b"], all, opts(18 * MIN)))).toEqual(["a", "b"]);
  });

  it("doesn't detach by time when the idle time is 0", () => {
    let k = keepSessions([], ["a"], all, opts(0, 5, 0));
    k = keepSessions(k, ["b"], all, opts(1, 5, 0));
    expect(ids(keepSessions(k, ["b"], all, opts(1000 * MIN, 5, 0)))).toEqual(["a", "b"]);
  });

  it("drops a session that no longer exists", () => {
    let k = keepSessions([], ["a"], all, opts(0));
    k = keepSessions(k, ["b"], all, opts(1));
    expect(ids(keepSessions(k, ["b"], new Set(["b"]), opts(2)))).toEqual(["b"]);
  });

  it("is the same list when nothing changed", () => {
    let k = keepSessions([], ["a"], all, opts(0));
    k = keepSessions(k, ["b"], all, opts(1));
    expect(keepSessions(k, ["b"], all, opts(2))).toBe(k);
  });
});
