// Light/dark: follow the system, or a choice remembered on this Mac.

import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

export type ThemeChoice = "system" | "light" | "dark";
const KEY = "workd.theme";
const NEXT: Record<ThemeChoice, ThemeChoice> = { system: "light", light: "dark", dark: "system" };

function stored(): ThemeChoice {
  try {
    const v = localStorage.getItem(KEY);
    if (v === "light" || v === "dark" || v === "system") return v;
  } catch {
    // Storage unavailable: follow the system.
  }
  return "system";
}

const media = () => window.matchMedia("(prefers-color-scheme: light)");

/** The choice, what it resolves to now, and a function to cycle it. */
export function useTheme(): { choice: ThemeChoice; resolved: "light" | "dark"; cycle: () => void } {
  const [choice, setChoice] = useState<ThemeChoice>(stored);
  const [systemLight, setSystemLight] = useState(() => media().matches);

  useEffect(() => {
    const m = media();
    const on = (e: MediaQueryListEvent) => setSystemLight(e.matches);
    m.addEventListener("change", on);
    return () => m.removeEventListener("change", on);
  }, []);

  const resolved = choice === "system" ? (systemLight ? "light" : "dark") : choice;
  useEffect(() => {
    document.documentElement.dataset.theme = resolved;
    try {
      localStorage.setItem(KEY, choice);
    } catch {
      // Not remembered; fine.
    }
    // The native title bar follows too.
    void getCurrentWindow()
      .setTheme(choice === "system" ? null : choice)
      .catch(() => {});
  }, [choice, resolved]);

  return { choice, resolved, cycle: () => setChoice((c) => NEXT[c]) };
}
