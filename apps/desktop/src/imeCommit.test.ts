import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { Terminal } from "@xterm/xterm";
import { takeImeCommits } from "./imeCommit";

let box: HTMLDivElement;
let term: Terminal;
let sent: string[];

const key = (type: "keydown" | "keyup", key: string, keyCode: number) => {
  const e = new KeyboardEvent(type, { key, bubbles: true, cancelable: true });
  Object.defineProperty(e, "keyCode", { get: () => keyCode });
  term.textarea!.dispatchEvent(e);
};
/** What an input method's commit does to xterm's textarea. */
const commit = (data: string, inputType = "insertText") => {
  const ta = term.textarea!;
  ta.dispatchEvent(new InputEvent("beforeinput", { inputType, data, bubbles: true, cancelable: true, composed: true }));
  ta.value += data;
  ta.dispatchEvent(new InputEvent("input", { inputType, data, bubbles: true, composed: true }));
};
const composition = (type: "compositionstart" | "compositionupdate" | "compositionend", data = "") =>
  term.textarea!.dispatchEvent(new CompositionEvent(type, { data, bubbles: true }));
/** xterm settles a keystroke on a timer; keys are pressed further apart than that. */
const settle = () => new Promise((r) => setTimeout(r, 10));

/** ？ with macOS Pinyin in WebKit: the character before its own keydown, Shift held. */
async function pinyinQuestionMark() {
  commit("？");
  key("keydown", "？", 229);
  key("keyup", "？", 191);
  await settle();
}

beforeEach(() => {
  box = document.createElement("div");
  document.body.appendChild(box);
  term = new Terminal();
  term.open(box);
  sent = [];
  term.onData((d) => sent.push(d));
});

afterEach(() => {
  term.dispose();
  box.remove();
});

describe("IME commits (D-126)", () => {
  it("loses the first shifted punctuation without it (xterm.js #6144; drop the fix once this fails)", async () => {
    key("keydown", "Shift", 16);
    await pinyinQuestionMark();
    await pinyinQuestionMark();
    expect(sent).toEqual(["？"]);
  });

  it("takes the first shifted punctuation, and the next, once each", async () => {
    const undo = takeImeCommits(box, term);
    key("keydown", "Shift", 16);
    await pinyinQuestionMark();
    expect(sent).toEqual(["？"]);
    await pinyinQuestionMark();
    expect(sent).toEqual(["？", "？"]);
    undo();
  });

  it("takes punctuation without Shift once", async () => {
    const undo = takeImeCommits(box, term);
    commit("，");
    key("keydown", "，", 229);
    key("keyup", "，", 188);
    await settle();
    expect(sent).toEqual(["，"]);
    undo();
  });

  it("sends a composition ended by shifted punctuation once", async () => {
    const undo = takeImeCommits(box, term);
    // "ni", then Shift+/ picks 你 and types ？ in the same keystroke.
    composition("compositionstart");
    composition("compositionupdate", "n");
    commit("n", "insertCompositionText");
    key("keydown", "n", 229);
    key("keyup", "n", 78);
    await settle();
    composition("compositionupdate", "ni");
    term.textarea!.value = "";
    commit("ni", "insertCompositionText");
    key("keydown", "i", 229);
    key("keyup", "i", 73);
    await settle();
    key("keydown", "Shift", 16);
    term.textarea!.value = "";
    commit("你", "insertFromComposition");
    composition("compositionend", "你");
    commit("？");
    key("keydown", "？", 229);
    key("keyup", "？", 191);
    await settle();
    expect(sent).toEqual(["你？"]);
    undo();
  });

  it("doesn't send twice when the character's keydown comes first", async () => {
    const undo = takeImeCommits(box, term);
    key("keydown", "Shift", 16);
    key("keydown", "？", 229);
    commit("？");
    key("keyup", "？", 191);
    await settle();
    expect(sent).toEqual(["？"]);
    undo();
  });
});
