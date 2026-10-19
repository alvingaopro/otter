// Punctuation an input method commits at once (D-126): with macOS Pinyin,
// ？！：（ and the like, typed with Shift. WebKit delivers the character
// before its own keydown (beforeinput, input, then keydown), so the last
// keydown xterm.js saw is the Shift's. xterm.js 6.0 drops typed text that
// comes while a keydown it saw isn't let go yet, so as not to send a key
// twice; the first press was lost, and the next, after the first's keyup, got
// through (xterm.js #6144). A modifier alone never types, so what it armed is
// lifted before xterm reads it, except around a composition: what one commits
// xterm sends itself once it ends, with whatever came after it in the textarea
// (a ？ that ended it too), and lifting the gate then would send that twice.
// Drop this once xterm.js takes #6200.

/** Keys that type nothing by themselves. */
const MODIFIERS = new Set(["Shift", "Control", "Alt", "Meta", "CapsLock"]);

/**
 * Lets text an input method commits in `box` reach `term` (an xterm.js
 * terminal opened in it) when the last key pressed was a modifier alone.
 * Returns what undoes it.
 */
export function takeImeCommits(box: HTMLElement, term: object): () => void {
  let modifierOnly = false;
  let composing = false;
  // In capture, on an ancestor of xterm's textarea: before xterm's own handlers.
  const down = (e: KeyboardEvent) => {
    modifierOnly = MODIFIERS.has(e.key);
  };
  const start = () => {
    composing = true;
  };
  const end = () => {
    composing = false;
    modifierOnly = false;
  };
  const before = (e: InputEvent) => {
    if (!modifierOnly || composing || e.inputType !== "insertText" || !e.data) return;
    // xterm's private gate; if a later xterm renames it, nothing happens.
    const core = (term as { _core?: { _keyDownSeen?: unknown } })._core;
    if (core && typeof core._keyDownSeen === "boolean") core._keyDownSeen = false;
  };
  box.addEventListener("keydown", down, true);
  box.addEventListener("compositionstart", start, true);
  box.addEventListener("compositionend", end, true);
  box.addEventListener("beforeinput", before, true);
  return () => {
    box.removeEventListener("keydown", down, true);
    box.removeEventListener("compositionstart", start, true);
    box.removeEventListener("compositionend", end, true);
    box.removeEventListener("beforeinput", before, true);
  };
}
