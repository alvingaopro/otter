// The app's own update (D-125): a newer release on offer, and how far
// installing it has got. Checking and installing are src-tauri's
// `updates.rs`; this is what the sidebar makes of its events.

export interface UpdateOffer {
  version: string;
  /** Set while installing: the share downloaded (0–1), null while its size is unknown. */
  progress?: number | null;
  /** Why the last try failed. */
  error?: string;
}

export const installing = (u: UpdateOffer) => u.progress !== undefined && !u.error;

/** The offer after a check (`update`): a version, or none. An install under way stays. */
export function offered(current: UpdateOffer | null, version: string | null): UpdateOffer | null {
  if (current && installing(current)) return current;
  if (!version) return null;
  return current?.version === version ? current : { version };
}

/** After a step of the download (`update-progress`). */
export function progressed(current: UpdateOffer | null, downloaded: number, total?: number): UpdateOffer | null {
  return current && { version: current.version, progress: total ? Math.min(downloaded / total, 1) : null };
}

/** What the update button says. */
export function updateLabel(u: UpdateOffer): string {
  if (u.error) return "Update failed — try again";
  if (u.progress === undefined) return `Update to v${u.version}`;
  if (u.progress === null) return "Updating…";
  return u.progress >= 1 ? "Installing…" : `Updating… ${Math.floor(u.progress * 100)}%`;
}
