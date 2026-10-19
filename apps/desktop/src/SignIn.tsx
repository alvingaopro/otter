// Sign-in pages from hosts (D-121). A tool on a host opens a page on a site
// that isn't a provider built in: the window asks, in its corner, and the
// developer opens it once or always. Settings › Sign-in pages lists the
// sites allowed, kept on this Mac (`hosts.toml`), where the CLI finds them
// too.

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** A page waiting for a yes. */
export interface SignInAsk {
  host: string;
  requestId: string;
  site: string;
  /** Where the tool runs, by name, when known. */
  workspace?: string;
}

/** How long a host keeps a page for the Mac; the question goes with it. */
export const ASK_TTL_MS = 2 * 60 * 1000;

function Cross() {
  return (
    <svg width="10" height="10" viewBox="0 0 12 12" aria-hidden="true">
      <path d="M3 3l6 6M9 3l-6 6" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  );
}

/** The pages waiting, newest last, in the window's corner. */
export function SignInAsks({ asks, onDone }: { asks: SignInAsk[]; onDone: (requestId: string) => void }) {
  if (asks.length === 0) return null;
  return (
    <div className="sign-in-asks">
      {asks.map((a) => (
        <Ask key={a.requestId} ask={a} onDone={() => onDone(a.requestId)} />
      ))}
    </div>
  );
}

/** One page: it goes once opened; if it couldn't be, it says why. */
function Ask({ ask: a, onDone }: { ask: SignInAsk; onDone: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const open = async (always: boolean) => {
    setBusy(true);
    try {
      await invoke("sign_in_open", { host: a.host, requestId: a.requestId, site: a.site, always });
      onDone();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  };
  return (
    <section className="sign-in-ask" role="alertdialog" aria-label={`Sign in at ${a.site}?`}>
      <div className="sign-in-ask-head">
        <span className="sign-in-ask-title">Sign in at {a.site}?</span>
        <button className="icon-btn" aria-label={error ? "Close" : "Not now"} title={error ? "Close" : "Not now"} onClick={onDone}>
          <Cross />
        </button>
      </div>
      {error ? (
        <p className="error small">{error}</p>
      ) : (
        <>
          <p className="muted small">
            A tool {a.workspace ? `in ${a.workspace} ` : ""}on {a.host} wants to open this page in your browser.
          </p>
          <div className="sign-in-ask-actions">
            <button className="btn primary" disabled={busy} onClick={() => void open(false)}>
              Open
            </button>
            <button
              className="btn outline"
              disabled={busy}
              onClick={() => void open(true)}
              title={`From now on, ${a.site} opens without asking`}
            >
              Always open
            </button>
          </div>
        </>
      )}
    </section>
  );
}

/** Settings › Sign-in pages: the providers built in, and the sites allowed here. */
export function SignInSites() {
  const [sites, setSites] = useState<string[]>([]);
  const [builtIn, setBuiltIn] = useState<string[]>([]);
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void invoke<{ builtIn: string[]; sites: string[] }>("sign_in_sites")
      .then((s) => {
        setBuiltIn(s?.builtIn ?? []);
        setSites(s?.sites ?? []);
      })
      .catch(() => {});
  }, []);

  const add = async () => {
    if (!draft.trim()) return;
    try {
      setSites(await invoke<string[]>("sign_in_allow", { site: draft }));
      setDraft("");
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };
  const forget = async (site: string) => {
    try {
      setSites(await invoke<string[]>("sign_in_forget", { site }));
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <>
      {builtIn.length > 0 && (
        <section className="settings-group" aria-label="Built in">
          <h3>Built in</h3>
          <div className="settings-card sign-in-built-in">
            <ul className="sign-in-chips" aria-label="Providers built in">
              {builtIn.map((b) => (
                <li key={b} className="chip">
                  {b}
                </li>
              ))}
            </ul>
            <span className="settings-hint">Open without asking.</span>
          </div>
        </section>
      )}
      <section className="settings-group" aria-label="Your sites">
        <h3>Your sites</h3>
        <div className="settings-card">
          <ul className="sign-in-site-list" aria-label="Sites that open without asking">
            {sites.map((s) => (
              <li key={s}>
                <span className="mono">{s}</span>
                <span className="settings-hint">Opens without asking</span>
                <button className="icon-btn" aria-label={`Ask before opening ${s}`} title="Ask again" onClick={() => void forget(s)}>
                  <Cross />
                </button>
              </li>
            ))}
            {sites.length === 0 && <li className="settings-hint">None yet.</li>}
          </ul>
          <form
            className="sign-in-add"
            onSubmit={(e) => {
              e.preventDefault();
              void add();
            }}
          >
            <input
              className="settings-input"
              aria-label="Add a sign-in site"
              placeholder="Paste a link, or login.example.com"
              value={draft}
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              onChange={(e) => {
                setDraft(e.target.value);
                setError(null);
              }}
            />
            <button className="btn" type="submit" disabled={!draft.trim()}>
              Add
            </button>
          </form>
        </div>
        {error && <span className="error-text small">{error}</span>}
      </section>
      <p className="muted small">
        Other sites ask in the window's corner: Open, or Always open. *.example.com covers a site and its subdomains. The CLI uses this list too.
      </p>
    </>
  );
}
