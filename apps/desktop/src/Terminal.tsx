// An embedded terminal attached to one session. Mounting attaches, unmounting
// detaches (the session keeps running); switching sessions remounts, and
// tmux redraws the current screen on attach.

import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { ClipboardAddon, type IClipboardProvider } from "@xterm/addon-clipboard";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import "@xterm/xterm/css/xterm.css";
import type { AttachEvent } from "./types";

/**
 * Clipboard writes go through the native plugin: text copied in the session
 * (tmux sends it as OSC 52) arrives after the mouse-up, which the WebView's
 * own clipboard API refuses as not user-initiated. Programs may not read the
 * Mac clipboard this way.
 */
const clipboard: IClipboardProvider = {
  readText: () => "",
  writeText: (_selection, text) => writeText(text),
};

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
    term.loadAddon(new ClipboardAddon(undefined, clipboard));
    // ⌘C copies a local (Option-drag) selection; otherwise it goes to the
    // session as usual.
    term.attachCustomKeyEventHandler((e) => {
      if (e.type === "keydown" && e.metaKey && e.key === "c" && term.hasSelection()) {
        void writeText(term.getSelection());
        return false;
      }
      // Ctrl+V (not ⌘V): an image on the Mac's clipboard goes to the session
      // first, so Claude Code's paste finds it; then the key itself.
      if (e.type === "keydown" && e.ctrlKey && !e.metaKey && !e.altKey && e.key === "v") {
        void invoke("paste_image", { host, workspace, session })
          .catch(() => false)
          .finally(() => {
            if (id !== null) void invoke("attach_input", { id, data: "\x16" });
          });
        return false;
      }
      return true;
    });
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
    // A hidden terminal (another view in front, D-041) has no size: fitting
    // it then would shrink the session for everyone attached.
    const observer = new ResizeObserver(() => {
      if (el.current && el.current.offsetWidth > 0 && el.current.offsetHeight > 0) fit.fit();
    });
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
