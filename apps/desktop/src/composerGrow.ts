/** The message box grows with what is written in it (D-177), up to this
 * share of the conversation's height; past it, it scrolls and the
 * conversation stays visible. */
export const MAX_SHARE = 0.5;

/** The box's height for what is written in it, and whether it then scrolls. */
export function fitHeight(content: number, room: number): { height: number; scrolls: boolean } {
  const max = Math.round(room * MAX_SHARE);
  return { height: Math.min(content, max), scrolls: content > max };
}

/** Within this many pixels of the end, the conversation counts as at its end. */
const NEAR = 24;

/** Whether the conversation is scrolled to (about) its end. */
export function atEnd(el: HTMLElement): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= NEAR;
}
