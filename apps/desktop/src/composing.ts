import { useRef } from "react";
import type { CompositionEvent, KeyboardEvent } from "react";

// An input method (macOS Pinyin and the like) confirms a candidate with Enter.
// That Enter is the method's, not a submit: while composing, `isComposing` is
// set and `keyCode` is 229, but WebKit delivers the confirming keydown just
// after `compositionend`, when both have already cleared. So an Enter that
// follows the end of a composition within this long is still the method's.
export const COMPOSITION_GRACE_MS = 50;

/** Whether this keydown belongs to an input method rather than to the user. */
export function isImeKey(e: Pick<KeyboardEvent, "keyCode" | "nativeEvent">): boolean {
  return e.nativeEvent?.isComposing === true || e.keyCode === 229;
}

/**
 * For a text box where Enter submits: `enterSubmits(e)` says whether a keydown
 * is a real Enter, and the two handlers go on the box's `onCompositionStart`
 * and `onCompositionEnd`.
 */
export function useComposition() {
  const endedAt = useRef<number | null>(null);
  return {
    onCompositionStart: (_: CompositionEvent) => {
      endedAt.current = null;
    },
    onCompositionEnd: (_: CompositionEvent) => {
      endedAt.current = performance.now();
    },
    enterSubmits: (e: KeyboardEvent): boolean => {
      if (e.key !== "Enter" || e.shiftKey || isImeKey(e)) return false;
      const ended = endedAt.current;
      return ended === null || performance.now() - ended > COMPOSITION_GRACE_MS;
    },
  };
}
