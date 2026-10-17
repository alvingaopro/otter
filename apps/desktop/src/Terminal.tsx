// An embedded terminal attached to one session. Mounting attaches, unmounting
// detaches (the session keeps running); switching sessions remounts. The host
// sends the session's screen and history, then the program's own output, so
// this terminal keeps the history and the wheel scrolls it here (D-118). An
// older host draws the session's screen instead, and keeps the history.

import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { ClipboardAddon, type IClipboardProvider } from "@xterm/addon-clipboard";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { openUrl } from "@tauri-apps/plugin-opener";
import { logicalLine, urlAtColumn } from "./links";
import { fontFamily, fontSize, onFontFamily, onFontSize } from "./terminalFont";
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

/** Lines of history asked for on attach; the host may send fewer (D-118). */
const HISTORY = 2000;

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
  /** On screen (its workspace kept but not shown otherwise, D-100): takes the keyboard when it comes back. */
  active?: boolean;
}

/**
 * Queries a program sends its terminal (where the cursor is, what the
 * terminal can do, its colors) that xterm answers on its own. A terminal that
 * keeps the history gets the program's own output, but the session's terminal
 * has answered these already, and a second answer would arrive as typing.
 * The handlers go with the terminal.
 */
function leaveAnswersToTheSession(term: XTerm) {
  const p = term.parser;
  const answered = () => true;
  const asks = (data: string) => data.split(";").includes("?");
  p.registerCsiHandler({ final: "c" }, answered);
  p.registerCsiHandler({ prefix: ">", final: "c" }, answered);
  p.registerCsiHandler({ prefix: "=", final: "c" }, answered);
  p.registerCsiHandler({ final: "n" }, answered);
  p.registerCsiHandler({ prefix: "?", final: "n" }, answered);
  p.registerCsiHandler({ intermediates: "$", final: "p" }, answered);
  p.registerCsiHandler({ prefix: "?", intermediates: "$", final: "p" }, answered);
  // Window reports; other window operations go through.
  p.registerCsiHandler({ final: "t" }, (params) => {
    const op = params[0];
    return typeof op === "number" && op >= 11 && op <= 21;
  });
  p.registerDcsHandler({ intermediates: "$", final: "q" }, answered);
  for (const osc of [4, 10, 11, 12]) p.registerOscHandler(osc, asks);
  // A read of the clipboard; writes still reach it.
  p.registerOscHandler(52, (data) => data.split(";")[1] === "?");
}

/** The http(s) link under the pointer, if any. */
function linkAt(term: XTerm, e: MouseEvent): string | undefined {
  const screen = term.element?.querySelector(".xterm-screen");
  if (!screen) return undefined;
  const r = screen.getBoundingClientRect();
  if (r.width === 0 || r.height === 0) return undefined;
  const col = Math.floor(((e.clientX - r.left) / r.width) * term.cols);
  const row = Math.floor(((e.clientY - r.top) / r.height) * term.rows);
  if (col < 0 || row < 0 || col >= term.cols || row >= term.rows) return undefined;
  const buf = term.buffer.active;
  const rows = Array.from({ length: term.rows }, (_, i) => {
    const line = buf.getLine(buf.viewportY + i);
    return { text: line?.translateToString(false) ?? "", wrapped: line?.isWrapped ?? false };
  });
  const { text, offset } = logicalLine(rows, row);
  return urlAtColumn(text, offset + col);
}

export function Terminal({ host, workspace, session, onEnd, generation, label, where, dimmed, theme, active = true }: Props) {
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

  // Shown again: the keyboard is the terminal's.
  useEffect(() => {
    if (active) termRef.current?.focus();
  }, [active]);

  useEffect(() => {
    const term = new XTerm({
      fontFamily: fontFamily(),
      fontSize: fontSize(),
      lineHeight: 1.25,
      cursorBlink: true,
      scrollback: 5000,
      theme: themeRef.current === "light" ? LIGHT : DARK,
      allowProposedApi: false,
      // The wheel scrolls this terminal's history, unless the program asked
      // for the mouse (or an older host draws the screen: the host's then).
      // Option-drag selects here even so.
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

    // Held until the attach says what it is: in a stream, the session's
    // terminal answers the program's queries, from the first byte.
    let held: Uint8Array[] | null = [];
    const output = new Channel<ArrayBuffer | number[]>();
    output.onmessage = (data) => {
      const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : Uint8Array.from(data);
      if (held) held.push(bytes);
      else term.write(bytes);
    };
    const events = new Channel<AttachEvent>();
    events.onmessage = (e) => {
      if (disposed) return;
      if (e.type === "closed") onEndRef.current({ kind: "lost" });
      else if (e.reason === "exited") onEndRef.current({ kind: "exited", exitCode: e.exitCode });
      else if (e.reason === "ended") onEndRef.current({ kind: "ended" });
      else if (e.reason === "error") onEndRef.current({ kind: "error", message: e.message ?? "attach failed" });
    };

    invoke<{ id: number; history: boolean }>("attach_open", {
      host,
      workspace,
      session,
      cols: term.cols,
      rows: term.rows,
      history: HISTORY,
      output,
      events,
    })
      .then((opened) => {
        if (disposed) {
          void invoke("attach_close", { id: opened.id });
          return;
        }
        id = opened.id;
        if (opened.history) leaveAnswersToTheSession(term);
        for (const bytes of held ?? []) term.write(bytes);
        held = null;
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
    // ⌘= / ⌘- anywhere (D-106); a hidden terminal fits when it's shown.
    const unfont = onFontSize((n) => {
      term.options.fontSize = n;
      if (el.current && el.current.offsetWidth > 0 && el.current.offsetHeight > 0) fit.fit();
    });
    // The font chosen in Settings (D-116). The terminal measures its cells
    // and draws its glyphs once, with whatever font is loaded then; the
    // bundled one loads only when first used, so measured before that the
    // text spreads out. Measure and draw again when it has loaded. xterm
    // measures only when the option changes: a trailing space changes it
    // without changing the font. The change also gives the terminal glyphs
    // drawn from then on, and redraws it in full. Never clear the glyphs
    // (clearTextureAtlas): terminals with the same font share them, and
    // every other one, a kept workspace's too, would draw scraps of the
    // wrong ones until its cells changed.
    const refont = (family: string) => {
      term.options.fontFamily = term.options.fontFamily === family ? `${family} ` : family;
      if (el.current && el.current.offsetWidth > 0 && el.current.offsetHeight > 0) fit.fit();
    };
    const unfamily = onFontFamily((family) => {
      refont(family);
      void loaded(family).then(() => !disposed && refont(family));
    });
    void loaded(fontFamily()).then(() => !disposed && refont(fontFamily()));
    // ⌘-click a link to open it (D-106): the session has the mouse otherwise,
    // so neither the press nor its release reaches it.
    let swallowUp = false;
    const linkDown = (e: MouseEvent) => {
      if (!e.metaKey || e.button !== 0) return;
      const url = linkAt(term, e);
      if (!url) return;
      e.preventDefault();
      e.stopPropagation();
      swallowUp = true;
      void openUrl(url);
    };
    const linkUp = (e: MouseEvent) => {
      if (!swallowUp) return;
      swallowUp = false;
      e.preventDefault();
      e.stopPropagation();
    };
    const box = el.current!;
    box.addEventListener("mousedown", linkDown, true);
    box.addEventListener("mouseup", linkUp, true);

    return () => {
      disposed = true;
      observer.disconnect();
      unfont();
      unfamily();
      box.removeEventListener("mousedown", linkDown, true);
      box.removeEventListener("mouseup", linkUp, true);
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
        <span className="statusbar-left" title="⌘-click a link to open it · ⌘F finds in its history · ⌘= ⌘- text size">
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

/** The regular and bold faces of `family` at the terminals' size, loaded (or given up on). */
function loaded(family: string): Promise<unknown> {
  const fonts = document.fonts;
  if (!fonts) return Promise.resolve();
  const size = `${fontSize()}px ${family}`;
  return Promise.all([fonts.load(size), fonts.load(`600 ${size}`)]).catch(() => undefined);
}
