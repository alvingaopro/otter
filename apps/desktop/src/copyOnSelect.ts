// Copy on select (D-123): text selected in a terminal goes to the clipboard
// when the mouse is let go, as it did when tmux had the mouse. Since D-118 the
// app's terminal selects by itself, and only ⌘C copied.

/** What's needed of the terminal. */
export interface Selectable {
  hasSelection(): boolean;
  getSelection(): string;
  onSelectionChange(listener: () => void): { dispose(): void };
}

/**
 * Copies the selection a press in `box` made, when the button is let go
 * (wherever that is). A selection left from before isn't copied again, so a
 * click never takes back the clipboard. Returns what undoes it.
 */
export function copyOnSelect(box: HTMLElement, term: Selectable, write: (text: string) => unknown): () => void {
  let pressed = false;
  let changed = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const selecting = term.onSelectionChange(() => {
    changed = true;
  });
  // Before the terminal's own mouse-down: a double-click selects a word there.
  const down = (e: MouseEvent) => {
    if (e.button !== 0) return;
    pressed = true;
    changed = false;
  };
  const up = (e: MouseEvent) => {
    if (e.button !== 0 || !pressed) return;
    pressed = false;
    // After the terminal's own mouse-up, which ends the selection.
    timer = setTimeout(() => {
      if (!changed || !term.hasSelection()) return;
      const text = term.getSelection();
      if (text) void Promise.resolve(write(text)).catch(() => {});
    });
  };
  box.addEventListener("mousedown", down, true);
  window.addEventListener("mouseup", up);
  return () => {
    clearTimeout(timer);
    selecting.dispose();
    box.removeEventListener("mousedown", down, true);
    window.removeEventListener("mouseup", up);
  };
}
