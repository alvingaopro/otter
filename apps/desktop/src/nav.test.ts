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

  it("starts on the Inbox and remembers the last view", () => {
    expect(storedView()).toBe("inbox");
    saveView("workspaces");
    expect(storedView()).toBe("workspaces");
    saveView("tasks");
    expect(storedView()).toBe("tasks");
    // Remembered before D-107.
    localStorage.setItem("otter.view", "features");
    expect(storedView()).toBe("tasks");
  });

  it("ignores an unknown stored view", () => {
    localStorage.setItem("otter.view", "settings");
    expect(storedView()).toBe("inbox");
    expect(parseView(null)).toBeUndefined();
  });

  it("maps ⌘1–⌘4 and nothing else", () => {
    expect(viewForShortcut(key("1", { metaKey: true }))).toBe("inbox");
    expect(viewForShortcut(key("2", { metaKey: true }))).toBe("tasks");
    expect(viewForShortcut(key("3", { metaKey: true }))).toBe("workspaces");
    expect(viewForShortcut(key("4", { metaKey: true }))).toBe("otter");
    expect(viewForShortcut(key("2"))).toBeUndefined();
    expect(viewForShortcut(key("2", { metaKey: true, shiftKey: true }))).toBeUndefined();
    expect(viewForShortcut(key("5", { metaKey: true }))).toBeUndefined();
  });

  it("moves with arrows, wrapping, and jumps with Home/End", () => {
    expect(viewForArrow("inbox", "ArrowDown")).toBe("tasks");
    expect(viewForArrow("tasks", "ArrowDown")).toBe("workspaces");
    expect(viewForArrow("workspaces", "ArrowDown")).toBe("otter");
    expect(viewForArrow("otter", "ArrowDown")).toBe("inbox");
    expect(viewForArrow("inbox", "ArrowUp")).toBe("otter");
    expect(viewForArrow("workspaces", "Home")).toBe("inbox");
    expect(viewForArrow("inbox", "End")).toBe("otter");
    expect(viewForArrow("tasks", "Enter")).toBeUndefined();
  });
});
