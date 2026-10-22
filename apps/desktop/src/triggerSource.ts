// Where the Automations view gets the hosts' triggers (D-083, D-185): each
// connected host's otterd, reloaded when it says they changed
// (`TriggersChanged`) or its event stream restarted. A host whose otterd
// predates triggers (or this list) has none; one that fails to answer once
// keeps what it last said.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { PlacedTrigger, Trigger } from "./automations";

export interface TriggerSource {
  setHosts(hosts: string[]): void;
  list(): PlacedTrigger[];
  /** Called with the new list whenever anything changes. Returns an unsubscribe. */
  subscribe(listener: (list: PlacedTrigger[]) => void): () => void;
  /** Pause (`false`) or resume (`true`). */
  setEnabled(key: string, enabled: boolean): Promise<void>;
  /**
   * Fire it now, by hand, for `occurrence` (an ISO time taken at the click):
   * the daemon starts one task per occurrence however often it's sent.
   * The trigger as it is then: its last firing says what came of it.
   */
  run(key: string, occurrence: string): Promise<Trigger>;
  remove(key: string): Promise<void>;
}

const split = (key: string) => {
  const i = key.indexOf("/");
  return { host: key.slice(0, i), trigger: key.slice(i + 1) };
};

export function daemonTriggers(): TriggerSource {
  const byHost = new Map<string, Trigger[]>();
  let hosts: string[] = [];
  let list: PlacedTrigger[] = [];
  const listeners = new Set<(l: PlacedTrigger[]) => void>();

  const rebuild = () => {
    list = hosts.flatMap((host) => (byHost.get(host) ?? []).map((trigger) => ({ host, trigger, key: `${host}/${trigger.id}` })));
    listeners.forEach((l) => l(list));
  };
  const load = async (host: string) => {
    try {
      byHost.set(host, await invoke<Trigger[]>("triggers_list", { host }));
    } catch {
      // Too old for triggers: none. A hiccup: what it said last stays.
    }
    if (hosts.includes(host)) rebuild();
  };
  /** Put a trigger the daemon just returned in place, ahead of the event. */
  const put = (host: string, t: Trigger) => {
    const list = byHost.get(host) ?? [];
    byHost.set(host, list.some((x) => x.id === t.id) ? list.map((x) => (x.id === t.id ? t : x)) : [...list, t]);
    rebuild();
  };

  void listen<{ host: string; record: { type: string } }>("host-event", (e) => {
    const { host, record } = e.payload;
    if (hosts.includes(host) && record.type === "TriggersChanged") void load(host);
  });
  void listen<{ host: string }>("host-resync", (e) => {
    if (hosts.includes(e.payload.host)) void load(e.payload.host);
  });

  return {
    setHosts(next) {
      const added = next.filter((h) => !hosts.includes(h));
      const changed = added.length > 0 || next.length !== hosts.length;
      // A host gone is read afresh if it comes back.
      for (const h of hosts) if (!next.includes(h)) byHost.delete(h);
      hosts = next;
      added.forEach((h) => void load(h));
      if (changed) rebuild();
    },
    list: () => list,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    async setEnabled(key, enabled) {
      const { host, trigger } = split(key);
      put(host, await invoke<Trigger>("trigger_update", { host, trigger, enabled }));
    },
    async run(key, occurrence) {
      const { host, trigger } = split(key);
      const t = await invoke<Trigger>("trigger_run", { host, trigger, occurrence });
      put(host, t);
      return t;
    },
    async remove(key) {
      const { host, trigger } = split(key);
      await invoke("trigger_delete", { host, trigger });
      byHost.set(
        host,
        (byHost.get(host) ?? []).filter((t) => t.id !== trigger),
      );
      rebuild();
    },
  };
}

/** Sample triggers, for tests and previews: nothing they do runs anything. */
export function mockTriggers(host = "preview", now = Date.now()): TriggerSource {
  const iso = (minutes: number) => new Date(now + minutes * 60_000).toISOString();
  let list: PlacedTrigger[] = [
    {
      host,
      key: `${host}/trg_nightly`,
      trigger: {
        id: "trg_nightly",
        name: "nightly",
        workspace_id: "ws_demo",
        title: "Nightly checks",
        request: "Run the full test suite and fix what broke overnight.",
        schedule: { kind: "daily", at: "20:20" },
        policy: { publish: true, max_minutes: 30, wait_limit_secs: 1800 },
        enabled: true,
        created_at: iso(-7 * 24 * 60),
        next_at: iso(240),
        history: [
          { occurrence: iso(-1200), at: iso(-1200), task_id: "ft_night1" },
          { occurrence: iso(-2640), at: iso(-2640), skipped: "“Nightly checks” is still at work" },
        ],
      },
    },
    {
      host,
      key: `${host}/trg_deps`,
      trigger: {
        id: "trg_deps",
        name: "deps-check",
        workspace_id: "ws_demo",
        title: "Dependency updates",
        request: "Look for outdated dependencies and update the safe ones.",
        schedule: { kind: "every", minutes: 720 },
        policy: {},
        enabled: false,
        created_at: iso(-3 * 24 * 60),
        history: [],
      },
    },
  ];
  const listeners = new Set<(l: PlacedTrigger[]) => void>();
  const changed = () => listeners.forEach((l) => l(list));
  return {
    setHosts() {},
    list: () => list,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    async setEnabled(key, enabled) {
      list = list.map((p) => (p.key === key ? { ...p, trigger: { ...p.trigger, enabled } } : p));
      changed();
    },
    async run(key, occurrence) {
      const p = list.find((x) => x.key === key)!;
      const trigger = { ...p.trigger, history: [...p.trigger.history, { occurrence, at: occurrence, task_id: "ft_mockrun" }] };
      list = list.map((x) => (x.key === key ? { ...x, trigger } : x));
      changed();
      return trigger;
    },
    async remove(key) {
      list = list.filter((p) => p.key !== key);
      changed();
    },
  };
}
