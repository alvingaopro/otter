import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Dialog } from "./Dialog";

/** Install `otter` (and `otterd`) for this user, matching this app's version. */
export function CliDialog({ version, onClose }: { version?: string; onClose: () => void }) {
  const [state, setState] = useState<{ busy: boolean; output?: string; error?: string }>({ busy: false });

  async function install() {
    setState({ busy: true });
    try {
      setState({ busy: false, output: await invoke<string>("install_cli") });
    } catch (e) {
      setState({ busy: false, error: String(e) });
    }
  }

  // Once per opening (development mode runs effects twice).
  const started = useRef(false);
  useEffect(() => {
    if (started.current) return;
    started.current = true;
    void install();
  }, []);

  return (
    <Dialog title="Command line tools" onClose={onClose}>
      <p className="muted">
        Installs <code>otter</code> and <code>otterd</code> {version} into <code>~/.local/bin</code>.
      </p>
      {state.busy && <div className="notice">Downloading…</div>}
      {state.output && <pre className="output">{state.output}</pre>}
      {state.output?.includes("not on your PATH") && (
        <p className="muted small">
          Add it to your shell: <code>export PATH="$HOME/.local/bin:$PATH"</code>
        </p>
      )}
      {state.error && <div className="notice error">{state.error}</div>}
      <div className="form-actions">
        <span className="spacer" />
        {state.error && (
          <button className="btn outline" onClick={() => void install()}>
            Try again
          </button>
        )}
        <button className="btn" onClick={onClose} disabled={state.busy}>
          Done
        </button>
      </div>
    </Dialog>
  );
}
