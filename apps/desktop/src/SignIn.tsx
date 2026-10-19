// Sign-in pages from hosts (D-121). A tool on a host opens a page on a site
// that isn't a provider built in: the window asks, in its corner, and the
// developer opens it once or always. Settings › This Mac lists the sites
// allowed, kept on this Mac (`hosts.toml`), where the CLI finds them too.

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
  const open = (a: SignInAsk, always: boolean) => {
    onDone(a.requestId);
    void invoke("sign_in_open", { host: a.host, requestId: a.requestId, site: a.site, always }).catch(() => {});
  };
  return (
    <div className="sign-in-asks">
      {asks.map((a) => (
        <section key={a.requestId} className="sign-in-ask" role="alertdialog" aria-label={`Sign in at ${a.site}?`}>
          <div className="sign-in-ask-head">
            <span className="sign-in-ask-title">Sign in at {a.site}?</span>
            <button className="icon-btn" aria-label="Not now" title="Not now" onClick={() => onDone(a.requestId)}>
              <Cross />
            </button>
          </div>
          <p className="muted small">
            A tool {a.workspace ? `in ${a.workspace} ` : ""}on {a.host} wants to open this page in your browser.
          </p>
          <div className="sign-in-ask-actions">
            <button className="btn primary" onClick={() => open(a, false)}>
              Open
            </button>
            <button className="btn outline" onClick={() => open(a, true)} title={`From now on, ${a.site} opens without asking`}>
              Always open
            </button>
          </div>
        </section>
      ))}
    </div>
  );
}

/** Settings › This Mac: the sites that open without asking. */
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
    <div className="sign-in-sites">
      {sites.length > 0 && (
        <ul className="sign-in-site-list" aria-label="Sites that open without asking">
          {sites.map((s) => (
            <li key={s} className="chip mono">
              {s}
              <button className="icon-btn" aria-label={`Ask before opening ${s}`} title="Ask again" onClick={() => void forget(s)}>
                <Cross />
              </button>
            </li>
          ))}
        </ul>
      )}
      <form
        className="sign-in-add"
        onSubmit={(e) => {
          e.preventDefault();
          void add();
        }}
      >
        <input
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
      {error && <span className="error small">{error}</span>}
      <span className="muted small">
        {builtIn.length > 0 && `Built in: ${builtIn.join(", ")}. `}
        Other sites ask first. Use *.example.com for a site and its subdomains.
      </span>
    </div>
  );
}
