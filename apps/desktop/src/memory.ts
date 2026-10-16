// Otter's memory (D-113): what Otter noted from what the developer decided —
// one for each conversation (a task's, or Otter's on the host) and one across
// every conversation on a host. Every turn is given both; the developer reads
// and corrects them here.

import { invoke } from "@tauri-apps/api/core";

/** Mirrors `otter_protocol::otter::OtterMemory`. */
export interface OtterMemory {
  host: string;
  conversation: string;
  max_chars: number;
}

export type MemoryScope = "host" | "conversation";

export interface MemorySource {
  /** The host's memory and the conversation's: the task's, or without one Otter's on the host. */
  get(host: string, task?: string): Promise<OtterMemory>;
  /** All of what one memory says; `previous` is what it said when the edit began. */
  set(host: string, task: string | undefined, memory: MemoryScope, text: string, previous: string): Promise<OtterMemory>;
}

export function daemonMemory(): MemorySource {
  return {
    get: (host, task) => invoke<OtterMemory>("memory_get", { host, task: task ?? null }),
    set: (host, task, memory, text, previous) =>
      invoke<OtterMemory>("memory_set", { host, task: task ?? null, memory, text, previous }),
  };
}

/** Memories kept here, for previews and tests. */
export function mockMemory(): MemorySource {
  const kept = new Map<string, string>();
  const key = (host: string, memory: MemoryScope, task?: string) =>
    memory === "host" ? `${host}` : `${host}/${task ?? "otter"}`;
  const get = async (host: string, task?: string): Promise<OtterMemory> => ({
    host: kept.get(key(host, "host")) ?? "",
    conversation: kept.get(key(host, "conversation", task)) ?? "",
    max_chars: 8000,
  });
  return {
    get,
    async set(host, task, memory, text, previous) {
      const k = key(host, memory, task);
      if ((kept.get(k) ?? "").trim() !== previous.trim()) throw new Error("Otter changed this memory since you opened it; look again");
      kept.set(k, text.trim() ? `${text.trim()}\n` : "");
      return get(host, task);
    },
  };
}
