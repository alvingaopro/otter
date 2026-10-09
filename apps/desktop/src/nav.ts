// The app's top-level views and the Activity Bar's keyboard model (D-041).
// Switching views only changes what is in front: nothing here may stop,
// detach or restart anything.

export type View = "features" | "workspaces";

export interface ViewEntry {
  id: View;
  label: string;
  /** ⌘ + this digit switches to it. */
  digit: string;
}

/** Top to bottom: Features is the primary entry. */
export const VIEWS: ViewEntry[] = [
  { id: "features", label: "Features", digit: "1" },
  { id: "workspaces", label: "Workspaces", digit: "2" },
];

const KEY = "otter.view";

export function parseView(v: string | null | undefined): View | undefined {
  return VIEWS.some((e) => e.id === v) ? (v as View) : undefined;
}

/** The view remembered on this Mac; Features the first time. */
export function storedView(): View {
  try {
    return parseView(localStorage.getItem(KEY)) ?? "features";
  } catch {
    return "features";
  }
}

export function saveView(view: View) {
  try {
    localStorage.setItem(KEY, view);
  } catch {
    // Not remembered; fine.
  }
}

/** The view a ⌘-digit shortcut asks for, if it is one. */
export function viewForShortcut(e: { metaKey: boolean; ctrlKey: boolean; altKey: boolean; shiftKey: boolean; key: string }): View | undefined {
  if (!e.metaKey || e.ctrlKey || e.altKey || e.shiftKey) return undefined;
  return VIEWS.find((v) => v.digit === e.key)?.id;
}

/** Roving focus in the bar: arrows move, Home/End jump; undefined for other keys. */
export function viewForArrow(current: View, key: string): View | undefined {
  const i = VIEWS.findIndex((v) => v.id === current);
  const n = VIEWS.length;
  switch (key) {
    case "ArrowDown":
    case "ArrowRight":
      return VIEWS[(i + 1) % n].id;
    case "ArrowUp":
    case "ArrowLeft":
      return VIEWS[(i - 1 + n) % n].id;
    case "Home":
      return VIEWS[0].id;
    case "End":
      return VIEWS[n - 1].id;
    default:
      return undefined;
  }
}
