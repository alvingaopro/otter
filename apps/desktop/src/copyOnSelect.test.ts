import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { copyOnSelect } from "./copyOnSelect";

let box: HTMLDivElement;
let selection = "";
let listeners: (() => void)[] = [];
const term = {
  hasSelection: () => selection !== "",
  getSelection: () => selection,
  onSelectionChange: (l: () => void) => {
    listeners.push(l);
    return { dispose: () => (listeners = listeners.filter((x) => x !== l)) };
  },
};
const select = (text: string) => {
  selection = text;
  listeners.forEach((l) => l());
};
const press = (target: EventTarget, type: "mousedown" | "mouseup", button = 0) =>
  target.dispatchEvent(new MouseEvent(type, { button, bubbles: true }));

beforeEach(() => {
  vi.useFakeTimers();
  selection = "";
  listeners = [];
  box = document.createElement("div");
  document.body.appendChild(box);
});

afterEach(() => {
  box.remove();
  vi.useRealTimers();
});

describe("copy on select (D-123)", () => {
  it("copies what a drag selected when the button is let go, even outside", () => {
    const copied: string[] = [];
    const undo = copyOnSelect(box, term, (t) => copied.push(t));
    press(box, "mousedown");
    select("cargo test -p otterd");
    expect(copied).toEqual([]);
    press(document.body, "mouseup");
    vi.runAllTimers();
    expect(copied).toEqual(["cargo test -p otterd"]);
    undo();
  });

  it("copies a word a double-click selected on the press itself", () => {
    const copied: string[] = [];
    const undo = copyOnSelect(box, term, (t) => copied.push(t));
    // The terminal's own listener, inside the box, selects as it's pressed.
    const inner = document.createElement("div");
    box.appendChild(inner);
    inner.addEventListener("mousedown", () => select("otterd"));
    press(inner, "mousedown");
    press(inner, "mouseup");
    vi.runAllTimers();
    expect(copied).toEqual(["otterd"]);
    undo();
  });

  it("never copies a selection left from before, another button, or a press elsewhere", () => {
    const copied: string[] = [];
    const undo = copyOnSelect(box, term, (t) => copied.push(t));
    select("left from before");
    // A click the program takes (it has the mouse): the selection stays.
    press(box, "mousedown");
    press(box, "mouseup");
    vi.runAllTimers();

    press(box, "mousedown", 2);
    select("by the right button");
    press(box, "mouseup", 2);
    press(document.body, "mousedown");
    select("elsewhere");
    press(document.body, "mouseup");
    vi.runAllTimers();
    expect(copied).toEqual([]);

    undo();
    press(box, "mousedown");
    select("after");
    press(box, "mouseup");
    vi.runAllTimers();
    expect(copied).toEqual([]);
  });
});
