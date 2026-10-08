// What happened in a workspace, newest first: its events from the host's log,
// then live ones as they arrive. After the host's stream restarts (reconnect,
// or the log can't serve the old cursor) it simply loads again.

import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Glyph } from "./Glyph";
import { ago } from "./model";
import { timelineRows, when, type EventRecord } from "./timeline";
import type { WorkspaceView } from "./types";

interface Loaded {
  records: EventRecord[];
  complete: boolean;
}

export function TimelinePanel({
  host,
  ws,
  now,
  onClose,
}: {
  host: string;
  ws: WorkspaceView;
  now: number;
  onClose: () => void;
}) {
  // By seq, so a reload and live events overlapping count once.
  const [records, setRecords] = useState<Map<number, EventRecord>>(new Map());
  const [complete, setComplete] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let live = true;
    setRecords(new Map());
    setLoading(true);
    // Live events since the current load started.
    let arrived: EventRecord[] = [];
    const add = (r: EventRecord) => {
      arrived.push(r);
      setRecords((m) => new Map(m).set(r.seq, r));
    };
    async function load() {
      arrived = [];
      try {
        const l = await invoke<Loaded>("workspace_events", { host, workspace: ws.id });
        if (!live) return;
        // A load replaces what was there (after a reset log, old seqs mean
        // nothing), plus what arrived live after the host answered it.
        const next = new Map(l.records.map((r) => [r.seq, r]));
        const last = l.records.length ? l.records[l.records.length - 1].seq : 0;
        for (const r of arrived) if (r.seq > last) next.set(r.seq, r);
        setRecords(next);
        setComplete(l.complete);
        setError(null);
      } catch (e) {
        if (live) setError(String(e));
      } finally {
        if (live) setLoading(false);
      }
    }
    // Listen first, then load: nothing falls between the two.
    const unEvent = listen<{ host: string; record: EventRecord }>("host-event", (e) => {
      if (e.payload.host === host && e.payload.record.workspace_id === ws.id) add(e.payload.record);
    });
    const unResync = listen<{ host: string }>("host-resync", (e) => {
      if (e.payload.host === host) void load();
    });
    void Promise.all([unEvent, unResync]).then(() => {
      if (live) void load();
    });
    return () => {
      live = false;
      void unEvent.then((f) => f());
      void unResync.then((f) => f());
    };
  }, [host, ws.id]);

  const rows = useMemo(() => timelineRows([...records.values()], ws), [records, ws]);

  return (
    <aside className="files timeline" aria-label="Timeline">
      <div className="files-head">
        <span className="files-title">Timeline</span>
        <button className="icon-btn" aria-label="Close timeline" title="Close" onClick={onClose}>
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M2 2l6 6M8 2l-6 6" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
        </button>
      </div>
      {error && <div className="notice error">{error}</div>}
      <ol className="timeline-list">
        {rows.map((r) => (
          <li key={r.seq} className="timeline-row">
            <Glyph kind={r.glyph} size={10} />
            <span className="timeline-text">
              {r.text}
              {r.detail && <span className="timeline-detail muted"> · {r.detail}</span>}
            </span>
            <time className="timeline-age muted" dateTime={r.ts} title={when(r.ts)}>
              {ago(r.ts, now) || "0s"}
            </time>
          </li>
        ))}
        {!loading && rows.length === 0 && !error && <li className="muted files-empty">Nothing recorded yet.</li>}
      </ol>
      {!loading && !complete && (
        <p className="files-hint muted">Earlier events are no longer in the host’s recent history.</p>
      )}
    </aside>
  );
}
