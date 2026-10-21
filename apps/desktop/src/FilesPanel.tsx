// A workspace's files on its host: browse, preview, download (save dialog),
// upload (button or drop files on the window), rename and delete (D-103).
// Transfers stream through otterd.

import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { ask, open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { bytes } from "./format";
import { highlightLines, languageOf } from "./highlight";
import { ContextMenu, Menu } from "./Menu";
import { explain } from "./newer";
import { RenameDialog } from "./RenameDialog";

interface Entry {
  name: string;
  kind: "file" | "dir" | "symlink" | "other";
  size: number;
  modified?: string;
}
interface Listing {
  path: string;
  parent?: string;
  root: string;
  entries: Entry[];
}
interface Transfer {
  id: string;
  name: string;
  done: number;
  total: number;
}

let nextId = 0;

/** The start of a file, as `file_preview` reads it. */
interface Preview {
  text?: string;
  size: number;
  truncated: boolean;
}

const HIDDEN_KEY = "otter.files.hidden";

function storedHidden(): boolean {
  try {
    return localStorage.getItem(HIDDEN_KEY) !== "false";
  } catch {
    return true;
  }
}

/** What the path field shows: below the root as `~ws/…`, else the path. */
export function shownPath(path: string, root: string): string {
  return path.startsWith(root) ? `~ws${path.slice(root.length)}` : path;
}

/** What to list for what was typed in the path field: `~ws/…` is below the root. */
export function typedPath(typed: string, root: string): string {
  const t = typed.trim();
  if (t === "~ws" || t.startsWith("~ws/")) return root + t.slice(3);
  return t;
}

export function FilesPanel({
  host,
  workspace,
  active = true,
  onClose,
}: {
  host: string;
  workspace: string;
  /** In front: only that panel takes files dropped on the window (D-100 keeps others mounted). */
  active?: boolean;
  onClose: () => void;
}) {
  const [listing, setListing] = useState<Listing | null>(null);
  const [path, setPath] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [transfers, setTransfers] = useState<Record<string, Transfer>>({});
  // The ids of the transfers this panel started.
  const own = useRef(new Set<string>());
  const [dropping, setDropping] = useState(false);
  // Dotfiles shown (as before) unless turned off; remembered on this Mac.
  const [hidden, setHidden] = useState(storedHidden);
  const [typed, setTyped] = useState<string | null>(null);
  const [preview, setPreview] = useState<{ entry: Entry; preview?: Preview } | null>(null);
  const [renaming, setRenaming] = useState<Entry | null>(null);
  const [menu, setMenu] = useState<{ entry: Entry; x: number; y: number } | null>(null);

  async function load(p = path) {
    try {
      const l = await invoke<Listing>("fs_list", { host, workspace, path: p });
      setListing(l);
      setPath(l.path);
      setPreview(null);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }

  useEffect(() => {
    void load("");
  }, [host, workspace]);

  useEffect(() => {
    // Only this panel's own: the others' (kept workspaces are mounted too,
    // D-100) would never be cleared from here.
    const un = listen<Transfer>("transfer", (e) => {
      if (own.current.has(e.payload.id)) setTransfers((t) => ({ ...t, [e.payload.id]: e.payload }));
    });
    return () => void un.then((f) => f());
  }, []);

  async function upload(sources: string[], dir: string) {
    if (sources.length === 0) return;
    const id = `up-${nextId++}`;
    own.current.add(id);
    setError(null);
    try {
      await invoke("file_upload", { host, workspace, dir, sources, overwrite: false, id });
    } catch (e) {
      const msg = String(e);
      if (msg.includes("already exists") && (await ask(`${msg}\n\nReplace it?`, { title: "Replace file?", kind: "warning" }))) {
        await invoke("file_upload", { host, workspace, dir, sources, overwrite: true, id }).catch((err) =>
          setError(String(err)),
        );
      } else {
        setError(msg);
      }
    }
    own.current.delete(id);
    setTransfers((t) => {
      const { [id]: _, ...rest } = t;
      return rest;
    });
    void load(dir);
  }

  // Files dropped on the window go into the folder shown here — only the
  // panel in front's: kept workspaces' panels are mounted too.
  useEffect(() => {
    if (!active) return;
    const un = getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === "over" || e.payload.type === "enter") setDropping(true);
      else if (e.payload.type === "leave") setDropping(false);
      else if (e.payload.type === "drop") {
        setDropping(false);
        void upload(e.payload.paths, path);
      }
    });
    return () => {
      setDropping(false);
      void un.then((f) => f());
    };
  }, [path, active]);

  function toggleHidden() {
    setHidden((h) => {
      try {
        localStorage.setItem(HIDDEN_KEY, String(!h));
      } catch {
        // Not remembered; fine.
      }
      return !h;
    });
  }

  async function open(entry: Entry) {
    setPreview({ entry });
    try {
      const p = await invoke<Preview>("file_preview", { host, workspace, path: `${path}/${entry.name}` });
      setPreview((cur) => (cur?.entry === entry ? { entry, preview: p } : cur));
    } catch (e) {
      setPreview(null);
      setError(explain(host, e));
    }
  }

  async function remove(entry: Entry) {
    const what = entry.kind === "dir" ? `${entry.name} and everything in it` : entry.name;
    if (!(await ask(`Delete ${what}? This can't be undone.`, { title: "Delete", kind: "warning" }))) return;
    try {
      await invoke("fs_remove", { host, workspace, path: `${path}/${entry.name}` });
      setPreview(null);
    } catch (e) {
      setError(explain(host, e));
    }
    void load(path);
  }

  async function download(entry: Entry) {
    const dest = await saveDialog({ defaultPath: entry.name });
    if (!dest) return;
    const id = `down-${nextId++}`;
    own.current.add(id);
    setError(null);
    try {
      await invoke("file_download", { host, workspace, path: `${path}/${entry.name}`, dest, id });
    } catch (e) {
      setError(String(e));
    }
    own.current.delete(id);
    setTransfers((t) => {
      const { [id]: _, ...rest } = t;
      return rest;
    });
  }

  async function pickAndUpload() {
    const picked = await openDialog({ multiple: true, directory: false });
    if (!picked) return;
    void upload(Array.isArray(picked) ? picked : [picked], path);
  }

  const shown = listing ? shownPath(listing.path, listing.root) : "";
  const entries = (listing?.entries ?? []).filter((e) => hidden || !e.name.startsWith("."));
  const actionsFor = (e: Entry) => [
    { label: "Download…", hidden: e.kind === "dir", onSelect: () => void download(e) },
    { label: "Rename…", onSelect: () => setRenaming(e) },
    { label: "Delete…", danger: true, onSelect: () => void remove(e) },
  ];

  return (
    <aside className={dropping ? "files dropping" : "files"} aria-label="Files">
      <div className="files-head">
        <span className="files-title">Files</span>
        <span className="files-actions">
          <button
            className={hidden ? "icon-btn files-hidden on" : "icon-btn files-hidden"}
            aria-pressed={hidden}
            aria-label="Show hidden files"
            title={hidden ? "Hide dotfiles" : "Show dotfiles"}
            onClick={toggleHidden}
          >
            .*
          </button>
          <button className="btn outline small-btn" onClick={() => void pickAndUpload()}>
            Upload…
          </button>
          <button className="icon-btn" aria-label="Close files" title="Close" onClick={onClose}>
            <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
              <path d="M2 2l6 6M8 2l-6 6" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
          </button>
        </span>
      </div>
      <div className="files-path">
        <button
          className="icon-btn"
          aria-label="Up one folder"
          title="Up"
          disabled={!listing?.parent}
          onClick={() => listing?.parent && void load(listing.parent)}
        >
          ↑
        </button>
        <input
          className="mono files-where files-path-input"
          aria-label="Folder"
          title={`${listing?.path ?? ""}\nType a folder (~ws/… is in the workspace) and press Return`}
          value={typed ?? shown}
          onChange={(e) => setTyped(e.target.value)}
          onBlur={() => setTyped(null)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && typed !== null && listing) {
              e.preventDefault();
              void load(typedPath(typed, listing.root)).then(() => setTyped(null));
            } else if (e.key === "Escape") {
              setTyped(null);
              e.currentTarget.blur();
            }
          }}
          spellCheck={false}
        />
        <button className="icon-btn" aria-label="Refresh" title="Refresh" onClick={() => void load()}>
          ↻
        </button>
      </div>
      {error && <div className="notice error">{error}</div>}
      {preview ? (
        <div className="file-preview">
          <div className="files-path">
            <button className="icon-btn" aria-label="Back to the folder" title="Back" onClick={() => setPreview(null)}>
              ←
            </button>
            <span className="mono files-where" title={preview.entry.name}>
              {preview.entry.name}
            </span>
            <button className="btn outline small-btn" onClick={() => void download(preview.entry)}>
              Download…
            </button>
            <Menu label={`${preview.entry.name} actions`} items={actionsFor(preview.entry).filter((i) => i.label !== "Download…")} />
          </div>
          {!preview.preview && <p className="muted files-empty">Reading…</p>}
          {preview.preview && preview.preview.text === undefined && (
            <p className="muted files-empty">Not text ({bytes(preview.preview.size)}): download it to open it.</p>
          )}
          {preview.preview?.text !== undefined && (
            <pre
              className="file-text hl"
              dangerouslySetInnerHTML={{ __html: highlightLines(preview.preview.text, languageOf(preview.entry.name)).join("\n") }}
            />
          )}
          {preview.preview?.truncated && preview.preview.text !== undefined && (
            <p className="files-hint muted">The first {bytes(preview.preview.text.length)} of {bytes(preview.preview.size)}.</p>
          )}
        </div>
      ) : (
        <ul className="files-list">
          {entries.map((e) => (
            <li key={e.name} className="files-item">
              <button
                className="files-row"
                onClick={() => (e.kind === "dir" ? void load(`${path}/${e.name}`) : void open(e))}
                onContextMenu={(ev) => {
                  ev.preventDefault();
                  setMenu({ entry: e, x: ev.clientX, y: ev.clientY });
                }}
                title={e.kind === "dir" ? `Open ${e.name}` : `Show ${e.name}`}
              >
                <span className="files-icon" aria-hidden="true">
                  {e.kind === "dir" ? "▸" : "·"}
                </span>
                <span className="files-name">{e.name}</span>
                <span className="files-size muted">{e.kind === "dir" ? "" : bytes(e.size)}</span>
              </button>
              <button
                className="icon-btn files-more"
                aria-label={`${e.name} actions`}
                title="Download, rename or delete"
                onClick={(ev) => {
                  const r = ev.currentTarget.getBoundingClientRect();
                  setMenu({ entry: e, x: r.right - 190, y: r.bottom + 2 });
                }}
              >
                ⋯
              </button>
            </li>
          ))}
          {listing && entries.length === 0 && <li className="muted files-empty">Empty folder</li>}
        </ul>
      )}
      {Object.values(transfers).map((t) => (
        <div key={t.id} className="transfer">
          <span>{t.name}</span>
          <span className="muted">
            {bytes(t.done)} / {bytes(t.total)}
          </span>
          <div className="meter">
            <div className="meter-fill" style={{ width: `${t.total ? (t.done / t.total) * 100 : 0}%` }} />
          </div>
        </div>
      ))}
      <p className="files-hint muted">Click a file to see it; ⋯ to download, rename or delete. Drop files on the window to upload them here.</p>
      {menu && <ContextMenu x={menu.x} y={menu.y} items={actionsFor(menu.entry)} onClose={() => setMenu(null)} />}
      {renaming && (
        <RenameDialog
          title={`Rename ${renaming.name}`}
          initial={renaming.name}
          onRename={async (name) => {
            try {
              await invoke("fs_rename", { host, workspace, path: `${path}/${renaming.name}`, name });
            } catch (e) {
              throw explain(host, e);
            }
            setPreview(null);
            void load(path);
          }}
          onClose={() => setRenaming(null)}
        />
      )}
    </aside>
  );
}
