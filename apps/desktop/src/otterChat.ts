// Otter's conversation on a host (D-093): the developer and Otter about any
// task there, apart from each task's own. `daemonOtter` talks to the hosts;
// `mockOtter` is for tests and previews.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { daemonMemory, mockMemory, type MemorySource } from "./memory";
import { commandId } from "./taskSource";
import type { Message } from "./tasks";

/** Mirrors `otter_core::task::OtterChat`. */
export interface OtterChat {
  messages: Message[];
  history_seq: number;
}

/** Otter's reply as it is written (`OtterStream`, transient). */
export interface OtterDraft {
  stream_id: string;
  text: string;
  done: boolean;
}

export interface OtterSource {
  get(host: string): Promise<OtterChat>;
  send(host: string, text: string): Promise<OtterChat>;
  /** Told when the host's conversation changes, and as a reply is written. */
  watch(host: string, onChange: () => void, onDraft: (d: OtterDraft) => void): () => void;
  /** Otter's memories (D-113). */
  memory?: MemorySource;
}

/** An otterd before Otter's conversation doesn't know the request. */
export function tooOld(error: unknown): boolean {
  return /unknown variant|invalid_request|InvalidRequest/.test(String(error));
}

export function daemonOtter(): OtterSource {
  return {
    memory: daemonMemory(),
    get: (host) => invoke<OtterChat>("otter_get", { host }),
    send: (host, text) => invoke<OtterChat>("otter_send", { host, commandId: commandId(), text }),
    watch(host, onChange, onDraft) {
      let stop: (() => void) | undefined;
      let gone = false;
      void listen<{ host: string; record: { type: string } & Partial<OtterDraft> }>("host-event", (e) => {
        if (e.payload.host !== host) return;
        const r = e.payload.record;
        if (r.type === "OtterChanged") onChange();
        if (r.type === "OtterStream") onDraft({ stream_id: r.stream_id ?? "", text: r.text ?? "", done: !!r.done });
      }).then((un) => {
        if (gone) un();
        else stop = un;
      });
      return () => {
        gone = true;
        stop?.();
      };
    },
  };
}

/** Sample conversation: Otter answers by listing what it was told. */
export function mockOtter(): OtterSource {
  const chats = new Map<string, OtterChat>();
  const watchers = new Map<string, Set<() => void>>();
  let n = 0;
  const stamp = () => new Date().toISOString();
  return {
    memory: mockMemory(),
    async get(host) {
      return chats.get(host) ?? { messages: [], history_seq: 0 };
    },
    async send(host, text) {
      const chat = chats.get(host) ?? { messages: [], history_seq: 0 };
      const mine: Message = { id: `msg_${++n}`, role: "user", text, at: stamp() };
      const reply: Message = {
        id: `msg_${++n}`,
        role: "controller",
        text: "Nothing is connected in this preview; on a host I'd look at its tasks.",
        at: stamp(),
        correlation_id: mine.id,
      };
      const next = { ...chat, messages: [...chat.messages, mine, reply] };
      chats.set(host, next);
      watchers.get(host)?.forEach((w) => w());
      return next;
    },
    watch(host, onChange) {
      const set = watchers.get(host) ?? new Set();
      set.add(onChange);
      watchers.set(host, set);
      return () => set.delete(onChange);
    },
  };
}
