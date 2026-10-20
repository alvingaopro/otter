// Settings › Git & GitHub (D-133): who commits in the repositories Otter
// manages, on every host, and each host's own key and GitHub sign-in. Only
// those repositories use them; each host's own git, ssh and gh stay as
// they are.

import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { openUrl } from "@tauri-apps/plugin-opener";

/** Mirrors `otter_protocol::host::GitView`. */
export interface GitView {
  name?: string;
  email?: string;
  ssh_key?: { public: string; fingerprint?: string };
  key_account?: string;
  github?: string;
  login?: { state: "pending"; code: string; url: string } | { state: "failed"; error: string };
}

interface CheckLine {
  ok: boolean;
  detail: string;
  account?: string;
}

/** Mirrors `otter_protocol::host::GitCheck`. */
export interface GitCheck {
  ssh: CheckLine;
  github: CheckLine;
  git: GitView;
}

/** A host's view: loading, too old for this, or as it says. */
type HostGit = { state: "loading" } | { state: "old" } | { state: "ready"; view: GitView };

const ADD_KEY_PAGE = "https://github.com/settings/ssh/new";

/**
 * `onUnsaved`: whether the identity has changes not saved yet, for the
 * dialog to ask before closing over them.
 */
export function GitSettings({ hosts, onUnsaved }: { hosts: string[]; onUnsaved?: (unsaved: boolean) => void }) {
  const [saved, setSaved] = useState({ name: "", email: "" });
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [saving, setSaving] = useState(false);
  const [sent, setSent] = useState<string | null>(null);
  const [views, setViews] = useState<Record<string, HostGit>>({});
  const [busy, setBusy] = useState<Record<string, string>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [checks, setChecks] = useState<Record<string, GitCheck>>({});
  // Hosts that "Set up all hosts" carries on with as each sign-in ends.
  const [carry, setCarry] = useState<Set<string>>(() => new Set());
  const [token, setToken] = useState("");
  const opened = useRef(new Set<string>());
  // Sign-ins started here: only theirs go to the clipboard and the browser
  // by themselves (not one begun from another Mac).
  const started = useRef(new Set<string>());

  useEffect(() => {
    void invoke<{ name?: string; email?: string }>("git_identity")
      .then((id) => {
        const v = { name: id?.name ?? "", email: id?.email ?? "" };
        setSaved(v);
        setName(v.name);
        setEmail(v.email);
      })
      .catch(() => {});
  }, []);

  async function load(host: string) {
    try {
      const view = await invoke<GitView | null>("git_view", { host });
      setViews((v) => ({ ...v, [host]: view ? { state: "ready", view } : { state: "old" } }));
      return view;
    } catch (e) {
      setErrors((x) => ({ ...x, [host]: String(e) }));
      return null;
    }
  }

  useEffect(() => {
    for (const h of hosts) void load(h);
  }, [hosts.join(",")]);

  // While a sign-in waits, ask again every two seconds.
  const waiting = hosts.filter((h) => {
    const v = views[h];
    return v?.state === "ready" && v.view.login?.state === "pending";
  });
  useEffect(() => {
    if (waiting.length === 0) return;
    const t = setInterval(() => waiting.forEach((h) => void load(h)), 2000);
    return () => clearInterval(t);
  }, [waiting.join(",")]);

  // A new code of a sign-in started here: on the clipboard, and GitHub's
  // page opened, once.
  useEffect(() => {
    for (const h of waiting) {
      if (!started.current.has(h)) continue;
      const v = views[h];
      if (v?.state !== "ready" || v.view.login?.state !== "pending") continue;
      const { code, url } = v.view.login;
      if (opened.current.has(code)) continue;
      opened.current.add(code);
      void writeText(code).catch(() => {});
      void openUrl(url).catch(() => {});
    }
  }, [views]);

  async function act<T>(host: string, label: string, run: () => Promise<T>): Promise<T | undefined> {
    setBusy((b) => ({ ...b, [host]: label }));
    setErrors((x) => {
      const { [host]: _, ...rest } = x;
      return rest;
    });
    try {
      return await run();
    } catch (e) {
      setErrors((x) => ({ ...x, [host]: String(e) }));
      return undefined;
    } finally {
      setBusy((b) => {
        const { [host]: _, ...rest } = b;
        return rest;
      });
      void load(host);
    }
  }

  const makeKey = (h: string) => act(h, "Making a key…", () => invoke<GitView>("git_key_create", { host: h, replace: false }));
  const signIn = (h: string) => {
    started.current.add(h);
    return act(h, "Starting…", () => invoke("github_login", { host: h, token: null }));
  };
  const signOut = (h: string) => act(h, "Signing out…", () => invoke("github_logout", { host: h }));
  const cancel = (h: string) => act(h, "Cancelling…", () => invoke("github_logout", { host: h }));
  const addKey = (h: string) =>
    act(h, "Adding the key…", async () => {
      const c = await invoke<GitCheck>("git_key_add", { host: h });
      setChecks((x) => ({ ...x, [h]: c }));
    });
  const check = (h: string) =>
    act(h, "Checking…", async () => {
      const c = await invoke<GitCheck>("git_check", { host: h });
      setChecks((x) => ({ ...x, [h]: c }));
    });

  // Carry on where "Set up all hosts" left off, as each sign-in ends.
  useEffect(() => {
    for (const h of carry) {
      const v = views[h];
      if (v?.state !== "ready" || busy[h]) continue;
      const g = v.view;
      if (g.github && g.ssh_key && !g.key_account) {
        setCarry((c) => new Set([...c].filter((x) => x !== h)));
        void addKey(h);
      } else if (g.key_account || g.login?.state === "failed") {
        setCarry((c) => new Set([...c].filter((x) => x !== h)));
      }
    }
  }, [views, carry]);

  async function setUpAll() {
    const todo = hosts.filter((h) => views[h]?.state === "ready");
    setCarry(new Set(todo));
    for (const h of todo) {
      let g = (views[h] as { view: GitView }).view;
      if (!g.ssh_key) g = (await makeKey(h)) ?? g;
      if (!g.github && g.login?.state !== "pending") await signIn(h);
    }
  }

  async function useToken() {
    const t = token.trim();
    if (!t) return;
    const todo = hosts.filter((h) => views[h]?.state === "ready");
    setToken("");
    setCarry(new Set(todo));
    await Promise.all(
      todo.map(async (h) => {
        // A key to add once signed in, as "Set up" makes one.
        if (!(views[h] as { view: GitView }).view.ssh_key) await makeKey(h);
        await act(h, "Signing in…", () => invoke("github_login", { host: h, token: t }));
      }),
    );
  }

  async function saveIdentity() {
    setSaving(true);
    setSent(null);
    try {
      const out = await invoke<{ host: string; error?: string }[]>("git_identity_set", { hosts, name, email });
      const failed = out.filter((o) => o.error);
      if (failed.length < out.length || out.length === 0) setSaved({ name: name.trim(), email: email.trim() });
      setSent(
        failed.length
          ? failed.map((f) => `${f.host}: ${f.error}`).join(" · ")
          : out.length
            ? `Saved on ${out.length === 1 ? out[0].host : `all ${out.length} hosts`}.`
            : "Saved; hosts get it as they connect.",
      );
    } catch (e) {
      setSent(String(e));
    } finally {
      setSaving(false);
    }
  }

  const changed = name.trim() !== saved.name || email.trim() !== saved.email;
  useEffect(() => onUnsaved?.(changed), [changed]);
  // Gone (another tab): its edit goes with it, so nothing waits to be saved.
  useEffect(() => () => onUnsaved?.(false), []);
  const ready = hosts.filter((h) => views[h]?.state === "ready");
  const allDone = ready.length > 0 && ready.every((h) => {
    const g = (views[h] as { view: GitView }).view;
    return !!g.key_account && !!g.github;
  });

  return (
    <>
      <section className="settings-group" aria-label="Identity">
        <h3>Identity</h3>
        <div className="settings-card">
          <div className="settings-row">
            <div className="settings-text">
              <div className="settings-label">Name</div>
            </div>
            <div className="settings-control">
              <input className="settings-input" aria-label="Git name" value={name} placeholder="Ada Lovelace" onChange={(e) => setName(e.target.value)} />
            </div>
          </div>
          <div className="settings-row">
            <div className="settings-text">
              <div className="settings-label">Email</div>
              <div className="settings-hint">Commits, pushes and pull requests in Otter's workspaces are yours.</div>
            </div>
            <div className="settings-control">
              <input
                className="settings-input"
                aria-label="Git email"
                value={email}
                placeholder="ada@example.com"
                spellCheck={false}
                autoCapitalize="off"
                onChange={(e) => setEmail(e.target.value)}
              />
            </div>
          </div>
          {(changed || sent) && (
            <div className="settings-row git-save">
              <span className="settings-hint" role="status">
                {sent ?? (hosts.length ? `Goes to ${hosts.join(", ")}.` : "Kept on this Mac until a host connects.")}
              </span>
              {changed && (
                <button type="button" className="btn primary" disabled={saving} onClick={() => void saveIdentity()}>
                  {saving ? "Saving…" : hosts.length > 1 ? "Save on all hosts" : "Save"}
                </button>
              )}
            </div>
          )}
        </div>
      </section>

      <section className="settings-group" aria-label="Hosts">
        <h3>SSH key and GitHub, per host</h3>
        {hosts.length === 0 ? (
          <p className="muted small">No host connected.</p>
        ) : (
          <div className="settings-card">
            {hosts.map((h) => (
              <HostRow
                key={h}
                host={h}
                git={views[h] ?? { state: "loading" }}
                busy={busy[h]}
                error={errors[h]}
                check={checks[h]}
                onMakeKey={() => void makeKey(h)}
                onSignIn={() => void signIn(h)}
                onSignOut={() => void signOut(h)}
                onCancel={() => void cancel(h)}
                onAddKey={() => void addKey(h)}
                onCheck={() => void check(h)}
              />
            ))}
          </div>
        )}
        {ready.length > 0 && !allDone && (
          <div className="git-setup">
            <button type="button" className="btn primary" disabled={Object.keys(busy).length > 0} onClick={() => void setUpAll()}>
              Set up {ready.length > 1 ? "all hosts" : ready[0]}
            </button>
            <span className="muted small">Makes a key on each, signs in to GitHub (a code per host), and adds the keys to your account.</span>
          </div>
        )}
        {ready.length > 0 && (
          <details className="git-token">
            <summary className="muted small">Sign in with a token instead</summary>
            <form
              className="sign-in-add"
              onSubmit={(e) => {
                e.preventDefault();
                void useToken();
              }}
            >
              <input
                type="password"
                className="settings-input"
                aria-label="GitHub token"
                placeholder="A fine-grained token: Contents, Pull requests (read and write)"
                autoComplete="off"
                value={token}
                onChange={(e) => setToken(e.target.value)}
              />
              <button type="submit" className="btn" disabled={!token.trim()}>
                {ready.length > 1 ? "Use on all hosts" : "Use"}
              </button>
            </form>
          </details>
        )}
      </section>
      <p className="muted small">
        Only the repositories Otter manages use these. Each host keeps its own git, ssh and gh; the agent and your terminals commit and push as
        you, and only Otter itself opens pull requests with the sign-in.
      </p>
    </>
  );
}

function HostRow({
  host,
  git,
  busy,
  error,
  check,
  onMakeKey,
  onSignIn,
  onSignOut,
  onCancel,
  onAddKey,
  onCheck,
}: {
  host: string;
  git: HostGit;
  busy?: string;
  error?: string;
  check?: GitCheck;
  onMakeKey: () => void;
  onSignIn: () => void;
  onSignOut: () => void;
  onCancel: () => void;
  onAddKey: () => void;
  onCheck: () => void;
}) {
  if (git.state !== "ready") {
    return (
      <div className="settings-row" aria-label={host}>
        <div className="settings-text">
          <div className="settings-label mono">{host}</div>
          <div className="settings-hint">{git.state === "old" ? `Update otterd on ${host} to set this up.` : "Loading…"}</div>
        </div>
      </div>
    );
  }
  const g = git.view;
  const pending = g.login?.state === "pending" ? g.login : null;
  const key = !g.ssh_key ? "No key" : g.key_account ? `Key on GitHub as ${g.key_account}` : "Key not on GitHub yet";
  const signedIn = g.github ? `Signed in as ${g.github}` : pending ? "Signing in…" : "Not signed in";
  // The one next step.
  const next = !g.ssh_key
    ? { label: "Make a key", run: onMakeKey }
    : !g.github && !pending
      ? { label: "Sign in to GitHub", run: onSignIn }
      : g.github && !g.key_account
        ? { label: "Add key to GitHub", run: onAddKey }
        : null;
  const keyFailed = !!error && !!g.ssh_key && !g.key_account && !!g.github;
  return (
    <div className="git-host" aria-label={host}>
      <div className="settings-row">
        <div className="settings-text">
          <div className="settings-label">
            <span className="mono">{host}</span>
            {g.key_account && g.github && <span className="key-state set">Ready</span>}
          </div>
          <div className="settings-hint">
            {key} · {signedIn}
            {g.github && (
              <>
                {" · "}
                <button type="button" className="link-btn" onClick={onSignOut} disabled={!!busy}>
                  Sign out
                </button>
              </>
            )}
          </div>
        </div>
        <div className="settings-control">
          <div className="settings-inline">
            {busy ? (
              <span className="muted small" role="status">
                {busy}
              </span>
            ) : next ? (
              <button type="button" className="btn outline" onClick={next.run}>
                {next.label}
              </button>
            ) : (
              !pending && (
                <button type="button" className="btn ghost" onClick={onCheck}>
                  Check
                </button>
              )
            )}
          </div>
        </div>
      </div>
      {pending && (
        <div className="git-code" role="status">
          <span className="muted small">Enter this code on GitHub (it's on your clipboard):</span>
          <code className="git-code-value">{pending.code}</code>
          <button type="button" className="btn ghost" onClick={() => void writeText(pending.code).catch(() => {})}>
            Copy
          </button>
          <button type="button" className="btn outline" onClick={() => void openUrl(pending.url).catch(() => {})}>
            Open GitHub
          </button>
          {/* Signed in already: "Sign out" is the way (it ends this too). */}
          {!g.github && (
            <button type="button" className="btn ghost" onClick={onCancel} disabled={!!busy}>
              Cancel
            </button>
          )}
        </div>
      )}
      {g.login?.state === "failed" && !pending && <p className="git-note error-text small">{g.login.error}</p>}
      {error && <p className="git-note error-text small">{error}</p>}
      {keyFailed && g.ssh_key && (
        <div className="git-key">
          <span className="muted small">Or add it on GitHub yourself:</span>
          <code className="git-key-value">{g.ssh_key.public}</code>
          <button type="button" className="btn ghost" onClick={() => void writeText(g.ssh_key!.public).catch(() => {})}>
            Copy
          </button>
          <button type="button" className="btn outline" onClick={() => void openUrl(ADD_KEY_PAGE).catch(() => {})}>
            Open GitHub
          </button>
        </div>
      )}
      {check && !busy && (
        <ul className="settings-test" aria-label={`Check of ${host}`}>
          {[check.ssh, check.github].map((l, i) => (
            <li key={i} className={`test-step ${l.ok ? "passed" : "failed"}`}>
              <span className="test-mark" aria-label={l.ok ? "passed" : "failed"}>
                {l.ok ? "✓" : "✕"}
              </span>
              <span>
                <span className="test-name">{i === 0 ? "SSH key" : "GitHub"}</span>
                <span className="muted small"> {l.detail}</span>
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
