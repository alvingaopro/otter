// A workspace's files on its host: browse, download (save dialog), upload
// (button or drop files on the window). Transfers stream through otterd.

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { ask, open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { bytes } from "./format";

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

export function FilesPanel({ host, workspace, onClose }: { host: string; workspace: string; onClose: () => void }) {
  const [listing, setListing] = useState<Listing | null>(null);
  const [path, setPath] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [transfers, setTransfers] = useState<Record<string, Transfer>>({});
  const [dropping, setDropping] = useState(false);

  async function load(p = path) {
    try {
      const l = await invoke<Listing>("fs_list", { host, workspace, path: p });
      setListing(l);
      setPath(l.path);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }

  useEffect(() => {
    void load("");
  }, [host, workspace]);

  useEffect(() => {
    const un = listen<Transfer>("transfer", (e) => setTransfers((t) => ({ ...t, [e.payload.id]: e.payload })));
    return () => void un.then((f) => f());
  }, []);

  async function upload(sources: string[], dir: string) {
    if (sources.length === 0) return;
    const id = `up-${nextId++}`;
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
    setTransfers((t) => {
      const { [id]: _, ...rest } = t;
      return rest;
    });
    void load(dir);
  }

  // Files dropped on the window go into the folder shown here.
  useEffect(() => {
    const un = getCurrentWebview().onDragDropEvent((e) => {
      if (e.payload.type === "over" || e.payload.type === "enter") setDropping(true);
      else if (e.payload.type === "leave") setDropping(false);
      else if (e.payload.type === "drop") {
        setDropping(false);
        void upload(e.payload.paths, path);
      }
    });
    return () => void un.then((f) => f());
  }, [path]);

  async function download(entry: Entry) {
    const dest = await saveDialog({ defaultPath: entry.name });
    if (!dest) return;
    const id = `down-${nextId++}`;
    setError(null);
    try {
      await invoke("file_download", { host, workspace, path: `${path}/${entry.name}`, dest, id });
    } catch (e) {
      setError(String(e));
    }
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

  const shown = listing
    ? listing.path.startsWith(listing.root)
      ? `~ws${listing.path.slice(listing.root.length)}` || "~ws"
      : listing.path
    : "";

  return (
    <aside className={dropping ? "files dropping" : "files"} aria-label="Files">
      <div className="files-head">
        <span className="files-title">Files</span>
        <span className="files-actions">
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
        <span className="mono files-where" title={listing?.path}>
          {shown}
        </span>
        <button className="icon-btn" aria-label="Refresh" title="Refresh" onClick={() => void load()}>
          ↻
        </button>
      </div>
      {error && <div className="notice error">{error}</div>}
      <ul className="files-list">
        {listing?.entries.map((e) => (
          <li key={e.name}>
            <button
              className="files-row"
              onClick={() => (e.kind === "dir" ? void load(`${path}/${e.name}`) : void download(e))}
              title={e.kind === "dir" ? `Open ${e.name}` : `Download ${e.name}`}
            >
              <span className="files-icon" aria-hidden="true">
                {e.kind === "dir" ? "▸" : "·"}
              </span>
              <span className="files-name">{e.name}</span>
              <span className="files-size muted">{e.kind === "dir" ? "" : bytes(e.size)}</span>
            </button>
          </li>
        ))}
        {listing && listing.entries.length === 0 && <li className="muted files-empty">Empty folder</li>}
      </ul>
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
      <p className="files-hint muted">Click a file to download it. Drop files on the window to upload them here.</p>
    </aside>
  );
}
