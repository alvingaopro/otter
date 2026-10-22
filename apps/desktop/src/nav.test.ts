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

  it("starts on Chat and remembers the last view", () => {
    expect(storedView()).toBe("tasks");
    saveView("workspaces");
    expect(storedView()).toBe("workspaces");
    saveView("tasks");
    expect(storedView()).toBe("tasks");
    // Remembered before D-107.
    localStorage.setItem("otter.view", "features");
    expect(storedView()).toBe("tasks");
    // The Inbox is gone (D-178): Chat instead.
    localStorage.setItem("otter.view", "inbox");
    expect(storedView()).toBe("tasks");
  });

  it("ignores an unknown stored view", () => {
    localStorage.setItem("otter.view", "settings");
    expect(storedView()).toBe("tasks");
    // Otter's view is gone (D-115).
    localStorage.setItem("otter.view", "otter");
    expect(storedView()).toBe("tasks");
    expect(parseView(null)).toBeUndefined();
  });

  it("maps ⌘1–⌘2 and nothing else", () => {
    expect(viewForShortcut(key("1", { metaKey: true }))).toBe("tasks");
    expect(viewForShortcut(key("2", { metaKey: true }))).toBe("workspaces");
    expect(viewForShortcut(key("3", { metaKey: true }))).toBeUndefined();
    expect(viewForShortcut(key("2"))).toBeUndefined();
    expect(viewForShortcut(key("2", { metaKey: true, shiftKey: true }))).toBeUndefined();
  });

  it("moves with arrows, wrapping, and jumps with Home/End", () => {
    expect(viewForArrow("tasks", "ArrowDown")).toBe("workspaces");
    expect(viewForArrow("workspaces", "ArrowDown")).toBe("tasks");
    expect(viewForArrow("tasks", "ArrowUp")).toBe("workspaces");
    expect(viewForArrow("workspaces", "Home")).toBe("tasks");
    expect(viewForArrow("tasks", "End")).toBe("workspaces");
    expect(viewForArrow("tasks", "Enter")).toBeUndefined();
  });
});
