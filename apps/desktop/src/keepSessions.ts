// Sessions that stay attached while another one is shown (D-152): a
// workspace's pane keeps the terminals of the sessions shown last, hidden,
// so going back to one is just showing it. How many, and how long one that
// isn't shown is kept, are this Mac's settings (Settings › Developer).

import { useEffect, useState } from "react";

const CAPACITY_KEY = "otter.kept-sessions";
const IDLE_KEY = "otter.kept-idle-minutes";

export const DEFAULT_KEPT = 5;
export const MAX_KEPT = 20;
export const DEFAULT_IDLE_MINUTES = 10;
/** A day; 0 keeps them until the capacity needs the place. */
export const MAX_IDLE_MINUTES = 1440;
/** How often a hidden terminal's time is looked at. */
const CHECK_MS = 15_000;

export interface KeepSettings {
  /** Sessions kept attached in a workspace, those shown included. */
  capacity: number;
  /** Minutes a session that isn't shown is kept; 0 for no limit. */
  idleMinutes: number;
}

function clamp(n: number, min: number, max: number, fallback: number): number {
  return Number.isFinite(n) ? Math.min(max, Math.max(min, Math.round(n))) : fallback;
}

function read(key: string, fallback: number, min: number, max: number): number {
  try {
    const v = localStorage.getItem(key);
    return v === null ? fallback : clamp(Number(v), min, max, fallback);
  } catch {
    return fallback;
  }
}

let settings: KeepSettings = {
  capacity: read(CAPACITY_KEY, DEFAULT_KEPT, 1, MAX_KEPT),
  idleMinutes: read(IDLE_KEY, DEFAULT_IDLE_MINUTES, 0, MAX_IDLE_MINUTES),
};
const listeners = new Set<(s: KeepSettings) => void>();

export function keepSettings(): KeepSettings {
  return settings;
}

export function setKeepSettings(next: Partial<KeepSettings>) {
  const merged = {
    capacity: clamp(next.capacity ?? settings.capacity, 1, MAX_KEPT, settings.capacity),
    idleMinutes: clamp(next.idleMinutes ?? settings.idleMinutes, 0, MAX_IDLE_MINUTES, settings.idleMinutes),
  };
  if (merged.capacity === settings.capacity && merged.idleMinutes === settings.idleMinutes) return;
  settings = merged;
  try {
    localStorage.setItem(CAPACITY_KEY, String(merged.capacity));
    localStorage.setItem(IDLE_KEY, String(merged.idleMinutes));
  } catch {
    // Not remembered; fine.
  }
  for (const f of listeners) f(merged);
}

export function useKeepSettings(): KeepSettings {
  const [s, setS] = useState(settings);
  useEffect(() => {
    listeners.add(setS);
    setS(settings);
    return () => void listeners.delete(setS);
  }, []);
  return s;
}

/** A kept session, and when it stopped being shown (null while it is). */
export interface Kept {
  id: string;
  hiddenAt: number | null;
}

/**
 * What stays attached after `shown` are in front: the ones shown, and the
 * others of `prev` that still exist (`valid`), haven't been hidden for the idle
 * limit, and fit in `capacity` with the ones shown, the longest hidden going
 * first. Kept in the order they were first kept, so the terminals aren't
 * moved around. `prev` itself when nothing changed.
 */
export function keepSessions(
  prev: Kept[],
  shown: string[],
  valid: ReadonlySet<string>,
  { now, capacity, idleMinutes }: { now: number } & KeepSettings,
): Kept[] {
  const was = new Map(prev.map((k) => [k.id, k.hiddenAt]));
  let next: Kept[] = [
    ...prev.map((k) => k.id),
    ...shown.filter((id) => !was.has(id)),
  ]
    .filter((id) => shown.includes(id) || valid.has(id))
    .map((id) => ({ id, hiddenAt: shown.includes(id) ? null : (was.get(id) ?? now) }));
  if (idleMinutes > 0) {
    next = next.filter((k) => k.hiddenAt === null || now - k.hiddenAt < idleMinutes * 60_000);
  }
  const room = Math.max(0, capacity - shown.length);
  const hidden = next.filter((k) => k.hiddenAt !== null);
  if (hidden.length > room) {
    // Oldest first; a tie loses to the one kept earlier (the sort is stable).
    const out = new Set(
      [...hidden]
        .sort((a, b) => a.hiddenAt! - b.hiddenAt!)
        .slice(0, hidden.length - room)
        .map((k) => k.id),
    );
    next = next.filter((k) => !out.has(k.id));
  }
  const same = next.length === prev.length && next.every((k, i) => k.id === prev[i].id && k.hiddenAt === prev[i].hiddenAt);
  return same ? prev : next;
}

/**
 * The sessions whose terminals a pane keeps mounted: those shown, and the
 * ones shown before, within the settings. Those shown come first in the
 * answer's own right, even before the effect has recorded them.
 */
export function useKeptSessions(shown: string[], valid: string[]): string[] {
  const s = useKeepSettings();
  const [kept, setKept] = useState<Kept[]>([]);
  const shownKey = shown.join(",");
  const validKey = valid.join(",");
  useEffect(() => {
    const step = () =>
      setKept((prev) => keepSessions(prev, shown, new Set(valid), { now: Date.now(), ...s }));
    step();
    const timer = setInterval(step, CHECK_MS);
    return () => clearInterval(timer);
  }, [shownKey, validKey, s.capacity, s.idleMinutes]);
  const ids = kept.map((k) => k.id);
  return [...ids, ...shown.filter((id) => !ids.includes(id))];
}
