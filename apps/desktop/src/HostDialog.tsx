import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";
import type { HostView } from "./types";

const STATUS: Record<HostView["status"], string> = {
  connecting: "Connecting…",
  connected: "Connected",
  unreachable: "Not reachable",
  incompatible: "Runs a otterd this app can’t talk to",
  not_installed: "otterd isn’t installed",
};

/** One host: its state, and installing/updating otterd or forgetting it. */
export function HostDialog({ host, version, onClose }: { host: HostView; version?: string; onClose: () => void }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [output, setOutput] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirmRemove, setConfirmRemove] = useState(false);

  const outdated = host.version !== undefined && version !== undefined && host.version !== version;
  const offerInstall = host.status === "not_installed" || host.status === "incompatible" || outdated;

  async function install() {
    setBusy(`Installing otterd ${version} on ${host.name}…`);
    setError(null);
    try {
      setOutput(await invoke<string>("host_install", { name: host.name }));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  async function remove() {
    try {
      await invoke("host_remove", { name: host.name });
      onClose();
    } catch (e) {
      setError(String(e));
    }
  }

  return (
    <Dialog title={host.name} onClose={onClose}>
      <dl className="facts">
        <dt>Reached by</dt>
        <dd className="mono">{host.describe}</dd>
        <dt>Status</dt>
        <dd>{STATUS[host.status]}</dd>
        {host.version && (
          <>
            <dt>otterd</dt>
            <dd className="mono">
              {host.version}
              {outdated && <span className="muted"> · this app is {version}</span>}
            </dd>
          </>
        )}
        <dt>Workspaces</dt>
        <dd>{host.workspaces.length}</dd>
      </dl>
      {host.message && host.status !== "connected" && <div className="notice">{host.message}</div>}
      {busy && <div className="notice">{busy}</div>}
      {output && <pre className="output">{output}</pre>}
      {error && <div className="notice error">{error}</div>}

      <div className="form-actions">
        {confirmRemove ? (
          <>
            <span className="muted small">Forget {host.name}? Nothing on it is touched; its sessions keep running.</span>
            <button className="btn outline" onClick={() => setConfirmRemove(false)}>
              Keep
            </button>
            <button className="btn danger" onClick={() => void remove()}>
              Remove
            </button>
          </>
        ) : (
          <>
            <button className="btn outline" onClick={() => setConfirmRemove(true)}>
              Remove host…
            </button>
            <span className="spacer" />
            {offerInstall && (
              <button className="btn primary" disabled={!!busy} onClick={() => void install()}>
                {host.status === "not_installed" ? `Install otterd ${version}` : `Update otterd to ${version}`}
              </button>
            )}
            <button className="btn" onClick={onClose}>
              Done
            </button>
          </>
        )}
      </div>
    </Dialog>
  );
}
