import { useEffect, useRef, useState, type FormEvent } from "react";
import { ComposerGrip, useComposerHeight } from "./composerResize";
import { MessageText, Speaker } from "./TasksView";
import { ago } from "./model";
import { tooOld, type OtterChat, type OtterDraft, type OtterSource } from "./otterChat";

type Loaded = { state: "loading" } | { state: "ready"; chat: OtterChat } | { state: "too_old" } | { state: "error"; error: string };

const HOST_KEY = "otter.chat.host";

function storedHost(hosts: string[]): string | undefined {
  try {
    const h = localStorage.getItem(HOST_KEY);
    if (h && hosts.includes(h)) return h;
  } catch {
    // Not remembered; fine.
  }
  return hosts[0];
}

/** The task ids a message names (`ft_…`), each once, in order. */
export function tasksNamed(text: string): string[] {
  return [...new Set(text.match(/\bft_[a-z0-9]+\b/g) ?? [])];
}

/**
 * Otter's conversation on a host (D-093): about any task there, apart from
 * each task's own. The host is chosen here and shown — a message goes to that
 * host's Otter only.
 */
export function OtterView({
  hosts,
  source,
  now,
  onOpenTask,
}: {
  hosts: string[];
  source: OtterSource;
  now: number;
  onOpenTask: (host: string, task: string) => void;
}) {
  const [host, setHost] = useState<string | undefined>(() => storedHost(hosts));
  const [loaded, setLoaded] = useState<Loaded>({ state: "loading" });
  const [draft, setDraft] = useState<OtterDraft | null>(null);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [inputHeight, setInputHeight] = useComposerHeight();
  const end = useRef<HTMLDivElement>(null);

  // A host that went away: the first one left.
  useEffect(() => {
    if (!host || !hosts.includes(host)) setHost(hosts[0]);
  }, [hosts, host]);

  useEffect(() => {
    if (!host) return;
    let live = true;
    const load = () =>
      source
        .get(host)
        .then((chat) => live && setLoaded({ state: "ready", chat }))
        .catch((e) => live && setLoaded(tooOld(e) ? { state: "too_old" } : { state: "error", error: String(e) }));
    setLoaded({ state: "loading" });
    setDraft(null);
    void load();
    const stop = source.watch(host, () => void load(), (d) => live && setDraft(d));
    return () => {
      live = false;
      stop();
    };
  }, [host, source]);

  const messages = (loaded.state === "ready" && loaded.chat.messages) || [];
  // A written reply shows until its message loads.
  const writing = draft && !(draft.done && messages.some((m) => m.role === "controller" && m.text.trim() === draft.text.trim()));
  const last = messages[messages.length - 1];
  const awaiting = !writing && last?.role === "user";
  useEffect(() => end.current?.scrollIntoView?.({ block: "end" }), [messages.length, draft?.text.length, awaiting]);

  function choose(h: string) {
    setHost(h);
    try {
      localStorage.setItem(HOST_KEY, h);
    } catch {
      // Not remembered; fine.
    }
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    const t = text.trim();
    if (!t || busy || !host) return;
    setBusy(true);
    setError(null);
    try {
      const chat = await source.send(host, t);
      setLoaded({ state: "ready", chat });
      setText("");
    } catch (err) {
      setError(tooOld(err) ? "This host's otterd is too old for Otter's conversation: update it." : String(err));
    } finally {
      setBusy(false);
    }
  }

  if (hosts.length === 0) {
    return (
      <main className="pane otter-pane" aria-label="Otter">
        <p className="muted">Connect a host first: Otter runs on a host, about the tasks there.</p>
      </main>
    );
  }

  return (
    <main className="pane otter-pane" aria-label="Otter">
      <header className="pane-head" data-tauri-drag-region>
        <h1 className="task-title">Otter</h1>
        <label className="otter-host">
          <span className="muted small">on</span>
          <select aria-label="Host" value={host} onChange={(e) => choose(e.target.value)}>
            {hosts.map((h) => (
              <option key={h} value={h}>
                {h}
              </option>
            ))}
          </select>
        </label>
      </header>
      <section className="conversation" aria-label="Conversation with Otter">
        <div className="messages" role="log" aria-live="polite">
          {loaded.state === "loading" && <p className="muted small">Loading…</p>}
          {loaded.state === "too_old" && (
            <p className="notice small">This host's otterd is too old for Otter's conversation: update it (Settings → Update command line tools, or `otter host install`).</p>
          )}
          {loaded.state === "error" && <p className="error-text small">{loaded.error}</p>}
          {loaded.state === "ready" && messages.length === 0 && (
            <p className="muted small">
              Ask Otter about the tasks on {host}, or for new work: “What needs me?”, “Fix the login bug in api, and start on it.”
            </p>
          )}
          {messages.map((m) => (
            <div key={m.id} className={`message ${m.role}`}>
              <div className="message-head">
                <Speaker role={m.role} />
                <span className="message-at">{ago(m.at, now)}</span>
              </div>
              <MessageText role={m.role} text={m.text} />
              {m.role === "controller" && tasksNamed(m.text).length > 0 && (
                <span className="chips">
                  {tasksNamed(m.text).map((t) => (
                    <button key={t} className="chip link-btn" onClick={() => host && onOpenTask(host, t)}>
                      Open {t}
                    </button>
                  ))}
                </span>
              )}
            </div>
          ))}
          {writing && draft && (
            <div className="message controller streaming">
              <div className="message-head">
                <Speaker role="controller" />
                <span className="message-at">{draft.done ? "now" : "writing…"}</span>
              </div>
              <MessageText role="controller" text={draft.text}>
                {!draft.done && <span className="cursor" aria-hidden="true" />}
              </MessageText>
            </div>
          )}
          {awaiting && <p className="muted small replying">Otter is replying…</p>}
          <div ref={end} />
        </div>
        {error && <p className="error-text small">{error}</p>}
        <form className="composer" onSubmit={submit}>
          <ComposerGrip height={inputHeight} onResize={setInputHeight} />
          <textarea
            aria-label="Message to Otter on this host"
            style={{ height: inputHeight }}
            placeholder={`Ask Otter about ${host}'s tasks, or for new work…`}
            value={text}
            disabled={loaded.state === "too_old"}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                void submit(e as unknown as FormEvent);
              }
            }}
          />
          <span className="composer-actions">
            <button className="btn primary" type="submit" disabled={busy || !text.trim() || loaded.state === "too_old"}>
              Send
            </button>
          </span>
        </form>
      </section>
    </main>
  );
}
