import { afterEach, describe, expect, it } from "vitest";
import { parseView, saveView, storedView, viewForArrow, viewForShortcut } from "./nav";

const key = (k: string, mods: Partial<{ metaKey: boolean; ctrlKey: boolean; altKey: boolean; shiftKey: boolean }> = {}) => ({
  metaKey: false,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  key: k,
  ...mods,
});

describe("nav", () => {
  afterEach(() => localStorage.clear());

  it("starts on Features and remembers the last view", () => {
    expect(storedView()).toBe("features");
    saveView("workspaces");
    expect(storedView()).toBe("workspaces");
  });

  it("ignores an unknown stored view", () => {
    localStorage.setItem("otter.view", "settings");
    expect(storedView()).toBe("features");
    expect(parseView(null)).toBeUndefined();
  });

  it("maps ⌘1/⌘2 and nothing else", () => {
    expect(viewForShortcut(key("1", { metaKey: true }))).toBe("features");
    expect(viewForShortcut(key("2", { metaKey: true }))).toBe("workspaces");
    expect(viewForShortcut(key("2"))).toBeUndefined();
    expect(viewForShortcut(key("2", { metaKey: true, shiftKey: true }))).toBeUndefined();
    expect(viewForShortcut(key("3", { metaKey: true }))).toBeUndefined();
  });

  it("moves with arrows, wrapping, and jumps with Home/End", () => {
    expect(viewForArrow("features", "ArrowDown")).toBe("workspaces");
    expect(viewForArrow("workspaces", "ArrowDown")).toBe("features");
    expect(viewForArrow("features", "ArrowUp")).toBe("workspaces");
    expect(viewForArrow("workspaces", "Home")).toBe("features");
    expect(viewForArrow("features", "End")).toBe("workspaces");
    expect(viewForArrow("features", "Enter")).toBeUndefined();
  });
});
