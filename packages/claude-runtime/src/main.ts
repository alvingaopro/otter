// otterd's Claude worker (D-057): one run per process, no network listener.
// Commands arrive on stdin, events leave on stdout (JSONL, bounded);
// diagnostics go to stderr. The first command must be `initialize`; nothing
// reaches Claude before `ready`.

import { existsSync, readFileSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { MAX_FRAME, PROTOCOL_VERSION, frame, parseCommand, type Command, type Event } from "./protocol.js";
import { Session, type QueryFn } from "./sdk.js";

export const WORKER_VERSION = "0.1.0";
/** How long the SDK may take to initialize. */
const INIT_TIMEOUT_MS = Number(process.env.OTTER_WORKER_INIT_TIMEOUT_MS) || 60_000;
const SHUTDOWN_GRACE_MS = 5_000;

export function sdkVersion(): string {
  try {
    const require = createRequire(import.meta.url);
    const entry = require.resolve("@anthropic-ai/claude-agent-sdk");
    return JSON.parse(readFileSync(join(dirname(entry), "package.json"), "utf8")).version ?? "unknown";
  } catch {
    return "unknown";
  }
}

/**
 * Whether the Claude Code the SDK runs is here for this platform: the SDK
 * ships it as a per-platform package (the glibc build first, then musl).
 */
export function bundledClaude(platform = process.platform, arch = process.arch): string | null {
  const require = createRequire(import.meta.url);
  const names =
    platform === "linux"
      ? [`linux-${arch}`, `linux-${arch}-musl`]
      : [`${platform}-${arch}`];
  for (const n of names) {
    try {
      const pkg = require.resolve(`@anthropic-ai/claude-agent-sdk-${n}/package.json`);
      return join(dirname(pkg), platform === "win32" ? "claude.exe" : "claude");
    } catch {
      // not installed for this platform
    }
  }
  return null;
}

/**
 * `main.js --check`: what this worker is, for installing and for otterd's
 * readiness report. Starts nothing and reads no credentials.
 */
export function check() {
  const claude = bundledClaude();
  const major = Number(process.versions.node.split(".")[0]);
  return {
    protocol_version: PROTOCOL_VERSION,
    worker_version: WORKER_VERSION,
    sdk_version: sdkVersion(),
    node_version: process.versions.node,
    node_supported: major >= MIN_NODE,
    claude_code: claude !== null && existsSync(claude),
  };
}

/** The oldest Node this worker supports (`engines` in package.json; the SDK asks for 18). */
export const MIN_NODE = 20;

/** Split a byte stream into frames; a frame over the limit is dropped and reported. */
export class Frames {
  private buf = "";
  private skipping = false;
  constructor(
    private onLine: (line: string) => void,
    private onTooLarge: () => void,
  ) {}

  push(chunk: string) {
    this.buf += chunk;
    for (;;) {
      const nl = this.buf.indexOf("\n");
      if (nl < 0) break;
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (this.skipping) {
        this.skipping = false;
        continue;
      }
      if (line.trim()) this.onLine(line);
    }
    if (Buffer.byteLength(this.buf, "utf8") > MAX_FRAME) {
      this.buf = "";
      if (!this.skipping) this.onTooLarge();
      this.skipping = true;
    }
  }
}

/** The worker, wired to `write` (stdout) and `exit`. Exported for tests. */
export function worker(write: (s: string) => void, exit: (code: number) => void, query?: QueryFn) {
  let session: Session | null = null;
  let closing = false;
  const emit = (ev: Event) => write(frame(ev));
  const ack = (request_id: string) => emit({ type: "ack", request_id });
  const nack = (request_id: string, error: string, category: "invalid_input" | "protocol_incompatible" = "invalid_input") =>
    emit({ type: "nack", request_id, error, category });

  const handle = async (c: Command) => {
    if (c.type === "initialize") {
      if (session) return nack(c.request_id, "already initialized");
      session = new Session(emit, c.payload, query);
      try {
        await session.start(INIT_TIMEOUT_MS);
      } catch (e) {
        emit({ type: "fatal", message: `Claude didn't start: ${(e as Error).message}`, category: "provider_unavailable" });
        return exit(1);
      }
      ack(c.request_id);
      return emit({
        type: "ready",
        protocol_version: PROTOCOL_VERSION,
        worker_version: WORKER_VERSION,
        sdk_version: sdkVersion(),
        node_version: process.versions.node,
      });
    }
    if (!session) return nack(c.request_id, "initialize first");
    try {
      switch (c.type) {
        case "send_turn":
          session.sendTurn(c.payload.turn_id, c.payload.text);
          return ack(c.request_id);
        case "interrupt":
          await session.interrupt(c.payload.turn_id);
          return ack(c.request_id);
        case "policy_reply":
          return session.policyReply(c.payload) ? ack(c.request_id) : nack(c.request_id, "no such check is waiting");
        case "resolve_interaction":
          return session.resolve(c.payload.request_id, c.payload.resolution)
            ? ack(c.request_id)
            : nack(c.request_id, "no such request is waiting");
        case "shutdown":
          closing = true;
          ack(c.request_id);
          await session.shutdown(SHUTDOWN_GRACE_MS);
          return exit(0);
      }
    } catch (e) {
      nack(c.request_id, (e as Error).message);
    }
  };

  // Commands are handled in order; a slow one (initialize) holds the rest.
  let chain = Promise.resolve();
  const frames = new Frames(
    (line) => {
      const parsed = parseCommand(line);
      if (!parsed.ok) {
        const id = (() => {
          try {
            return String(JSON.parse(line).request_id ?? "");
          } catch {
            return "";
          }
        })();
        const incompatible = parsed.error.startsWith("protocol");
        return nack(id, parsed.error, incompatible ? "protocol_incompatible" : "invalid_input");
      }
      const command = parsed.command;
      // Answers can't wait behind a command that waits for them.
      if (command.type === "policy_reply" || command.type === "resolve_interaction" || command.type === "interrupt") {
        void handle(command);
      } else {
        chain = chain.then(() => handle(command));
      }
    },
    () => emit({ type: "notice", message: `a command over ${MAX_FRAME} bytes was dropped` }),
  );
  return {
    input: (chunk: string) => frames.push(chunk),
    /** Stdin closed: otterd is gone, so the run ends. */
    end: async () => {
      if (closing) return;
      closing = true;
      await session?.shutdown(SHUTDOWN_GRACE_MS);
      exit(0);
    },
  };
}

/** Run as a program, not imported by tests: by real path (`/tmp` may be a symlink). */
function isMain(): boolean {
  if (!process.argv[1]) return false;
  try {
    return realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url));
  } catch {
    return false;
  }
}

if (isMain() && process.argv[2] === "--check") {
  const c = check();
  const code = c.node_supported && c.claude_code && c.sdk_version !== "unknown" ? 0 : 1;
  // Exit once it's written: stdout may be a pipe.
  process.stdout.write(JSON.stringify(c) + "\n", () => process.exit(code));
} else if (isMain()) {
  const w = worker(
    (s) => process.stdout.write(s),
    (code) => process.exit(code),
  );
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (chunk: string) => w.input(chunk));
  process.stdin.on("end", () => void w.end());
}
