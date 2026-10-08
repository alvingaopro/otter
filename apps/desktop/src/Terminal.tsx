// An embedded terminal attached to one session. Mounting attaches, unmounting
// detaches (the session keeps running); switching sessions remounts, and
// tmux redraws the current screen on attach.

import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { ClipboardAddon } from "@xterm/addon-clipboard";
import "@xterm/xterm/css/xterm.css";
import type { AttachEvent } from "./types";

const DARK = {
  background: "#0B0C0E",
  foreground: "#E8E6E1",
  cursor: "#E8E6E1",
  cursorAccent: "#0B0C0E",
  selectionBackground: "#2E3A4F",
  black: "#1B1C20",
  red: "#FF7B72",
  green: "#9FB8A0",
  yellow: "#F0A060",
  blue: "#6CA6FF",
  magenta: "#C79BF2",
  cyan: "#7DCFCF",
  white: "#C9C7C2",
  brightBlack: "#5A5D64",
  brightRed: "#FF9A92",
  brightGreen: "#B8D1B9",
  brightYellow: "#F5BA85",
  brightBlue: "#9CC2FF",
  brightMagenta: "#DAB8F7",
  brightCyan: "#A3E0E0",
  brightWhite: "#F2F0EC",
};

const LIGHT = {
  background: "#FBFAF7",
  foreground: "#1D1E21",
  cursor: "#1D1E21",
  cursorAccent: "#FBFAF7",
  selectionBackground: "#C9DCF7",
  black: "#1D1E21",
  red: "#C4352B",
  green: "#3E7A47",
  yellow: "#A25A12",
  blue: "#2160C4",
  magenta: "#8A3FB8",
  cyan: "#18797A",
  white: "#6E7178",
  brightBlack: "#5A5D64",
  brightRed: "#D9473C",
  brightGreen: "#4C8F56",
  brightYellow: "#B66A1C",
  brightBlue: "#2F6FD6",
  brightMagenta: "#9D52CC",
  brightCyan: "#1F8E8F",
  brightWhite: "#2E3036",
};

export type Ending =
  | { kind: "exited"; exitCode?: number }
  | { kind: "ended" }
  | { kind: "error"; message: string }
  | { kind: "lost" };

interface Props {
  host: string;
  workspace: string;
  session: string;
  /** Called when the attach ends on its own (not when we detach). */
  onEnd: (ending: Ending) => void;
  /** Bumped by the parent to attach again. */
  generation: number;
  /** Status line, left: what is attached. Right: where. */
  label: string;
  where: string;
  /** Dim the screen (the attach is over or lost). */
  dimmed: boolean;
  theme: "light" | "dark";
}

export function Terminal({ host, workspace, session, onEnd, generation, label, where, dimmed, theme }: Props) {
  const el = useRef<HTMLDivElement>(null);
  const onEndRef = useRef(onEnd);
  onEndRef.current = onEnd;
  const [size, setSize] = useState<string>("");
  const termRef = useRef<XTerm | null>(null);
  const themeRef = useRef(theme);
  themeRef.current = theme;

  // Switch colors in place; no reattach.
  useEffect(() => {
    if (termRef.current) termRef.current.options.theme = theme === "light" ? LIGHT : DARK;
  }, [theme]);

  useEffect(() => {
    const term = new XTerm({
      fontFamily: '"JetBrains Mono", "SF Mono", Menlo, monospace',
      fontSize: 13,
      lineHeight: 1.25,
      cursorBlink: true,
      scrollback: 5000,
      theme: themeRef.current === "light" ? LIGHT : DARK,
      allowProposedApi: false,
      // The session (tmux) handles the mouse: the wheel scrolls its history.
      // Option-drag selects locally instead.
      macOptionClickForcesSelection: true,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    // Text copied in the session (OSC 52) goes to the Mac clipboard.
    term.loadAddon(new ClipboardAddon());
    term.open(el.current!);
    termRef.current = term;
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch {
      // DOM renderer fallback.
    }
    fit.fit();
    term.focus();
    setSize(`${term.cols}×${term.rows}`);

    let id: number | null = null;
    let disposed = false;

    const output = new Channel<ArrayBuffer | number[]>();
    output.onmessage = (data) => {
      term.write(data instanceof ArrayBuffer ? new Uint8Array(data) : Uint8Array.from(data));
    };
    const events = new Channel<AttachEvent>();
    events.onmessage = (e) => {
      if (disposed) return;
      if (e.type === "closed") onEndRef.current({ kind: "lost" });
      else if (e.reason === "exited") onEndRef.current({ kind: "exited", exitCode: e.exitCode });
      else if (e.reason === "ended") onEndRef.current({ kind: "ended" });
      else if (e.reason === "error") onEndRef.current({ kind: "error", message: e.message ?? "attach failed" });
    };

    invoke<number>("attach_open", {
      host,
      workspace,
      session,
      cols: term.cols,
      rows: term.rows,
      output,
      events,
    })
      .then((attachId) => {
        if (disposed) void invoke("attach_close", { id: attachId });
        else id = attachId;
      })
      .catch((err) => {
        if (!disposed) onEndRef.current({ kind: "error", message: String(err) });
      });

    const input = term.onData((data) => {
      if (id !== null) void invoke("attach_input", { id, data });
    });
    const resized = term.onResize(({ cols, rows }) => {
      setSize(`${cols}×${rows}`);
      if (id !== null) void invoke("attach_resize", { id, cols, rows });
    });
    const observer = new ResizeObserver(() => fit.fit());
    observer.observe(el.current!);

    return () => {
      disposed = true;
      observer.disconnect();
      input.dispose();
      resized.dispose();
      if (id !== null) void invoke("attach_close", { id });
      termRef.current = null;
      term.dispose();
    };
  }, [host, workspace, session, generation]);

  return (
    <>
      <section className={dimmed ? "terminal-frame dimmed" : "terminal-frame"} aria-label="Terminal">
        <div className="terminal" ref={el} />
      </section>
      <div className="statusbar">
        <span className="statusbar-left">
          <span className={dimmed ? "dot off" : "dot"} />
          {dimmed ? "not attached" : "attached"} · {label}
        </span>
        <span>
          {where} · {size}
        </span>
      </div>
    </>
  );
}
