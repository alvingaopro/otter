import { invoke } from "@tauri-apps/api/core";

// Requests a host's otterd may be too old for: the app asks once per host
// and request, and then leaves that host alone instead of showing an error
// in every pane (a fleet is updated one host at a time).

/** An otterd that doesn't know the request it was sent. */
export function unknownToHost(error: unknown): boolean {
  return /unknown variant/.test(String(error));
}

const tooOld = new Map<string, Set<string>>();

/** Whether `host` was found too old for `command`. */
export function knownTooOld(host: string, command: string): boolean {
  return tooOld.get(host)?.has(command) ?? false;
}

/**
 * `invoke`, for a request newer hosts know: `undefined` once the host turned
 * out too old for it (remembered until the app restarts, or the host is
 * updated and reconnects — see `forgetHost`).
 */
export async function invokeNewer<T>(host: string, command: string, args: Record<string, unknown>): Promise<T | undefined> {
  if (knownTooOld(host, command)) return undefined;
  try {
    return await invoke<T>(command, args);
  } catch (e) {
    if (!unknownToHost(e)) throw e;
    const s = tooOld.get(host) ?? new Set<string>();
    s.add(command);
    tooOld.set(host, s);
    return undefined;
  }
}

/** A host that came back (maybe updated): ask it again. */
export function forgetHost(host: string) {
  tooOld.delete(host);
}

/** What to say when an action needs a newer otterd. */
export function explain(host: string, error: unknown): string {
  return unknownToHost(error) ? `${host}'s otterd is too old for this: update it from the host's page.` : String(error);
}
