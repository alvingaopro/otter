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
import { copyOnSelect } from "./copyOnSelect";
import { takeImeCommits } from "./imeCommit";
import { pixelScroll } from "./pixelScroll";
import { logicalLine, urlAtColumn } from "./links";
import { fontFamily, fontSize, onFontFamily, onFontSize } from "./terminalFont";
import "@xterm/xterm/css/xterm.css";
import type { AttachEvent } from "./types";
import { countTerminalBytes } from "./perf";

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
  background: "#FFFFFF",
  foreground: "#1D1E21",
  cursor: "#1D1E21",
  cursorAccent: "#FFFFFF",
  selectionBackground: "#CCE0FF",
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

/** On the light ground, a program's own colours that would wash out (a dark
 * theme's greys and yellows) are lifted to 4.5:1; dark draws them as they are
 * (D-176). */
const contrastFloor = (theme: "light" | "dark") => (theme === "light" ? 4.5 : 1);

/** Lines of history asked for on attach (the terminal's scrollback); the host
 * may send fewer (D-118). */
const HISTORY = 5000;
/** Lines of history to keep once earlier ones are asked for (D-119). */
const MORE_HISTORY = 20000;
/** On screen is said again this often: the host stops taking a terminal it
 *  hasn't heard that from in 90 s as on screen (D-137). */
const VIEW_AGAIN_MS = 30_000;
/** Smaller than this, the element is mid-layout, not a size to give the
 *  session: its program (Claude Code) lays its history out again at it. */
const MIN_COLS = 20;
const MIN_ROWS = 4;
/** The session takes this terminal's size once it has held this long. */
const RESIZE_SETTLE_MS = 150;

/** A size the terminal can really be shown at. */
export function realSize(d: { cols: number; rows: number } | undefined): boolean {
  return !!d && d.cols >= MIN_COLS && d.rows >= MIN_ROWS;
}

/** What the host says besides output (D-119). */
type Control =
  | { type: "window"; size: [number, number] | null }
  | { type: "snapshot"; lines: number; more: boolean }
  | { type: string };

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
  // Bumped to attach again: a new attach takes the window back (D-119).
  const [epoch, setEpoch] = useState(0);
  /** Another client sizes the window: this terminal shows it at that size. */
  const [following, setFollowing] = useState(false);
  /** Earlier history on the host: offered at the top of what's here. */
  const [earlier, setEarlier] = useState<"none" | "offer" | "loading">("none");
  const loadEarlier = useRef<() => void>(() => {});
  const termRef = useRef<XTerm | null>(null);
  const themeRef = useRef(theme);
  themeRef.current = theme;

  // Switch colors in place; no reattach.
  useEffect(() => {
    if (!termRef.current) return;
    termRef.current.options.theme = theme === "light" ? LIGHT : DARK;
    termRef.current.options.minimumContrastRatio = contrastFloor(theme);
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
      minimumContrastRatio: contrastFloor(themeRef.current),
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
    // ⌘C copies a selection here too; otherwise it goes to the session as
    // usual.
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
    if (realSize(fit.proposeDimensions())) fit.fit();
    term.focus();
    setSize(`${term.cols}×${term.rows}`);

    let id: number | null = null;
    let disposed = false;

    let controls = false;
    /** Another client's size for the window, while it sizes it. */
    let follow: [number, number] | null = null;
    /** On screen, as the host was last told (it assumes so). */
    let visible = true;
    /** Earlier history is on the host, and wasn't asked for in full yet. */
    let more = false;
    let askedAll = false;
    /** Earlier history asked for: how far from the end the reader was. */
    let fromBottom: number | null = null;

    const onScreen = () => !!el.current && el.current.offsetWidth > 0 && el.current.offsetHeight > 0;
    // WebGL only while on screen (D-153): WebKit has about sixteen contexts
    // and takes the oldest when there are more, and kept terminals (D-100,
    // D-152) would hold them hidden. Hidden, xterm draws nothing anyway.
    let webgl: { addon: WebglAddon; gl: WebGL2RenderingContext | null } | null = null;
    let noWebgl = false;
    // xterm's dispose takes the canvas away but leaves its context to the
    // garbage collector, still counted: give it up now.
    const release = () => {
      if (!webgl) return;
      const { addon, gl } = webgl;
      webgl = null;
      addon.dispose();
      gl?.getExtension("WEBGL_lose_context")?.loseContext();
    };
    const renderer = () => {
      if (!onScreen()) {
        release();
        return;
      }
      if (webgl || noWebgl) return;
      let addon: WebglAddon;
      try {
        addon = new WebglAddon();
      } catch {
        // No WebGL here: the DOM renderer.
        noWebgl = true;
        return;
      }
      const screen = term.element?.querySelector(".xterm-screen");
      const canvases = () => [...(screen?.querySelectorAll("canvas") ?? [])];
      const before = new Set(canvases());
      try {
        term.loadAddon(addon);
      } catch {
        // No context now: the DOM renderer, and another try when next shown.
        try {
          addon.dispose();
        } catch {
          // Never activated.
        }
        return;
      }
      // Its context: on the canvas it added that has one (the others are 2D).
      const gl =
        canvases()
          .filter((c) => !before.has(c))
          .map((c) => c.getContext("webgl2"))
          .find((g) => g !== null) ?? null;
      const mine = { addon, gl };
      webgl = mine;
      addon.onContextLoss(() => {
        if (webgl !== mine) return;
        release();
        // Lost while shown: a new one at once, once per loss.
        renderer();
      });
    };
    renderer();
    // Fit to the element; while another client sizes the window, stay at its
    // size, and tell the host what this one would be.
    const fitNow = () => {
      if (!onScreen()) return;
      const want = fit.proposeDimensions();
      if (!realSize(want)) return;
      if (!follow) {
        fit.fit();
        return;
      }
      if (want && id !== null) void invoke("attach_resize", { id, cols: want.cols, rows: want.rows });
    };
    // Off screen, the host sends nothing; back, it catches up (D-119).
    // Still on screen, it's said `again` now and then (D-137).
    const tellView = (again = false) => {
      if (id === null || !controls) return;
      const now = onScreen() && document.visibilityState !== "hidden";
      if (now === visible && !(again && now)) return;
      visible = now;
      const proposed = now ? fit.proposeDimensions() : undefined;
      const want = realSize(proposed) ? proposed : undefined;
      void invoke("attach_view", { id, visible: now, cols: want?.cols ?? term.cols, rows: want?.rows ?? term.rows });
    };
    const offerEarlier = () => {
      const buf = term.buffer.active;
      const atTop = buf.type === "normal" && buf.viewportY === 0 && buf.length > term.rows;
      setEarlier(fromBottom !== null ? "loading" : controls && more && !askedAll && atTop ? "offer" : "none");
    };
    loadEarlier.current = () => {
      if (id === null || fromBottom !== null) return;
      const buf = term.buffer.normal;
      fromBottom = buf.length - buf.viewportY;
      askedAll = true;
      term.options.scrollback = MORE_HISTORY;
      offerEarlier();
      void invoke("attach_history", { id, lines: MORE_HISTORY });
    };
    const apply = (c: Control) => {
      if (c.type === "window" && "size" in c) {
        follow = c.size;
        setFollowing(c.size !== null);
        if (c.size) term.resize(c.size[0], c.size[1]);
        else fitNow();
      } else if (c.type === "snapshot" && "more" in c) {
        more = c.more;
        if (fromBottom !== null) {
          // Rebuilt with earlier history: back where the reader was.
          term.scrollToLine(Math.max(0, term.buffer.normal.length - fromBottom));
          fromBottom = null;
        }
        offerEarlier();
      }
    };
    // In order with the output around it: a size takes effect before what's
    // laid out at it.
    const deliver = (item: Uint8Array | Control) => {
      if (item instanceof Uint8Array) term.write(item);
      else term.write("", () => !disposed && apply(item));
    };

    // Held until the attach says what it is: in a stream, the session's
    // terminal answers the program's queries, from the first byte.
    let held: (Uint8Array | Control)[] | null = [];
    const output = new Channel<ArrayBuffer | number[] | Control>();
    output.onmessage = (data) => {
      const item =
        data instanceof ArrayBuffer ? new Uint8Array(data) : Array.isArray(data) ? Uint8Array.from(data) : data;
      if (item instanceof Uint8Array) countTerminalBytes(item.byteLength);
      if (held) held.push(item);
      else deliver(item);
    };
    const events = new Channel<AttachEvent>();
    events.onmessage = (e) => {
      if (disposed) return;
      if (e.type === "closed") onEndRef.current({ kind: "lost" });
      else if (e.reason === "exited") onEndRef.current({ kind: "exited", exitCode: e.exitCode });
      else if (e.reason === "ended") onEndRef.current({ kind: "ended" });
      else if (e.reason === "error") onEndRef.current({ kind: "error", message: e.message ?? "attach failed" });
    };

    // Attached once it's on screen at a real size, measured in its own font,
    // never before: the first size the session gets is this terminal's, not
    // one of a layout still settling (the app starting, after an update) —
    // at each size it gets, Claude Code lays its history out again.
    let started = false;
    let fontReady = false;
    const start = () => {
      if (started || disposed || !fontReady || !onScreen() || !realSize(fit.proposeDimensions())) return;
      started = true;
      fit.fit();
      invoke<{ id: number; history: boolean; controls?: boolean }>("attach_open", {
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
          controls = opened.controls === true;
          if (opened.history) leaveAnswersToTheSession(term);
          for (const item of held ?? []) deliver(item);
          held = null;
          tellView();
        })
        .catch((err) => {
          if (!disposed) onEndRef.current({ kind: "error", message: String(err) });
        });
    };

    const input = term.onData((data) => {
      if (id !== null) void invoke("attach_input", { id, data });
    });
    // The session gets a size once the layout has settled: each one it gets,
    // its program may lay out its whole history again.
    let settle: number | undefined;
    const resized = term.onResize(({ cols, rows }) => {
      setSize(`${cols}×${rows}`);
      window.clearTimeout(settle);
      settle = window.setTimeout(() => {
        // Following another client's size isn't this terminal's own.
        if (id !== null && !follow && !disposed) void invoke("attach_resize", { id, cols: term.cols, rows: term.rows });
      }, RESIZE_SETTLE_MS);
    });
    const scrolled = term.onScroll(offerEarlier);
    // A hidden terminal (another view in front, D-041) has no size: fitting
    // it then would shrink the session for everyone attached.
    const observer = new ResizeObserver(() => {
      renderer();
      fitNow();
      start();
      tellView();
    });
    observer.observe(el.current!);
    const onVisibility = () => tellView();
    document.addEventListener("visibilitychange", onVisibility);
    const viewAgain = window.setInterval(() => tellView(true), VIEW_AGAIN_MS);
    // ⌘= / ⌘- anywhere (D-106); a hidden terminal fits when it's shown.
    const unfont = onFontSize((n) => {
      term.options.fontSize = n;
      fitNow();
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
      fitNow();
    };
    const unfamily = onFontFamily((family) => {
      refont(family);
      void loaded(family).then(() => !disposed && refont(family));
    });
    void loaded(fontFamily()).then(() => {
      if (disposed) return;
      fontReady = true;
      refont(fontFamily());
      start();
    });
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
    // Clicked while another client sizes the window: attach again, which
    // takes the window back.
    // Not a ⌘-click on a link or an Option-drag to select.
    const reclaim = (e: MouseEvent) => {
      if (follow && e.button === 0 && !e.metaKey && !e.altKey) setEpoch((n) => n + 1);
    };
    const box = el.current!;
    box.addEventListener("mousedown", linkDown, true);
    box.addEventListener("mouseup", linkUp, true);
    box.addEventListener("mousedown", reclaim);
    // A selection made here is copied when the mouse is let go (D-123).
    const uncopy = copyOnSelect(box, term, writeText);
    // Pinyin's ？！：（ arrive on the first press (D-126).
    const untake = takeImeCommits(box, term);
    // The history follows the finger, not a row at a time (D-130).
    const unscroll = pixelScroll(term);

    return () => {
      disposed = true;
      observer.disconnect();
      unfont();
      unfamily();
      box.removeEventListener("mousedown", linkDown, true);
      box.removeEventListener("mouseup", linkUp, true);
      box.removeEventListener("mousedown", reclaim);
      uncopy();
      untake();
      unscroll();
      document.removeEventListener("visibilitychange", onVisibility);
      window.clearInterval(viewAgain);
      window.clearTimeout(settle);
      input.dispose();
      resized.dispose();
      scrolled.dispose();
      setFollowing(false);
      setEarlier("none");
      if (id !== null) void invoke("attach_close", { id });
      termRef.current = null;
      // Unmounted while shown (a restart, a reconnect): its context too.
      release();
      term.dispose();
    };
  }, [host, workspace, session, generation, epoch]);

  return (
    <>
      <section className={dimmed ? "terminal-frame dimmed" : "terminal-frame"} aria-label="Terminal">
        <div className="terminal" ref={el} />
        {earlier !== "none" && (
          <button
            className="terminal-earlier"
            disabled={earlier === "loading"}
            onClick={() => loadEarlier.current()}
          >
            {earlier === "loading" ? "Loading earlier lines…" : "Load earlier lines"}
          </button>
        )}
      </section>
      <div className="statusbar">
        <span className="statusbar-left" title="⌘-click a link to open it · ⌘F finds in its history · ⌘= ⌘- text size">
          <span className={dimmed ? "dot off" : "dot"} />
          {dimmed ? "not attached" : "attached"} · {label}
        </span>
        <span title={following ? "Another window sized this session. Click the terminal to fit it here." : undefined}>
          {where} · {size}
          {following && " · sized by another window"}
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
