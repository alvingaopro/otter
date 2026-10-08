import { useEffect, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Dialog } from "./Dialog";
import { Menu } from "./Menu";
import { LineChart, Meter, Sparkline } from "./charts";
import { bytes, percent, rate, uptime } from "./format";
import type { Direction, ForwardView, HostHistory, HostMetrics, HostView, ListeningPort } from "./types";

type Range = "live" | "1h" | "24h" | "7d" | "30d";
const RANGES: [Range, string][] = [
  ["live", "10 min"],
  ["1h", "1 hour"],
  ["24h", "24 hours"],
  ["7d", "7 days"],
  ["30d", "30 days"],
];
const SPAN_MS: Record<Exclude<Range, "live">, number> = {
  "1h": 3600e3,
  "24h": 86400e3,
  "7d": 7 * 86400e3,
  "30d": 30 * 86400e3,
};

const STATUS: Record<HostView["status"], string> = {
  connecting: "Connecting…",
  connected: "Connected",
  unreachable: "Not reachable",
  incompatible: "Runs an otterd this app can’t talk to",
  not_installed: "otterd isn’t installed",
};

/** A host: how busy it is, its ports, and installing or forgetting it. */
export function HostPage({
  host,
  version,
  forwards,
  onRemoved,
}: {
  host: HostView;
  version?: string;
  forwards: ForwardView[];
  onRemoved: () => void;
}) {
  const [metrics, setMetrics] = useState<HostMetrics | null>(null);
  const [metricsError, setMetricsError] = useState<string | null>(null);
  const [ports, setPorts] = useState<ListeningPort[] | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [output, setOutput] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirmRemove, setConfirmRemove] = useState(false);
  const [mapping, setMapping] = useState<{ direction: Direction; port?: number } | null>(null);
  const [range, setRange] = useState<Range>("live");
  const [history, setHistory] = useState<HostHistory | null>(null);

  const connected = host.status === "connected";
  const outdated = host.version !== undefined && version !== undefined && host.version !== version;
  const offerInstall = host.status === "not_installed" || host.status === "incompatible" || outdated;
  const ssh = host.describe.startsWith("ssh ");

  // Usage every interval and ports every 10 s, while the page is open.
  useEffect(() => {
    if (!connected) return;
    let live = true;
    const load = async () => {
      try {
        const m = await invoke<HostMetrics>("host_metrics", { host: host.name });
        if (live) {
          setMetrics(m);
          setMetricsError(null);
        }
      } catch (e) {
        if (live) setMetricsError(String(e));
      }
    };
    const loadPorts = async () => {
      try {
        const p = await invoke<ListeningPort[]>("host_ports", { host: host.name });
        if (live) setPorts(p);
      } catch {
        if (live) setPorts(null);
      }
    };
    void load();
    void loadPorts();
    const a = setInterval(load, 5000);
    const b = setInterval(loadPorts, 10000);
    return () => {
      live = false;
      clearInterval(a);
      clearInterval(b);
    };
  }, [host.name, connected]);

  // Recorded history for the chosen range, refreshed every minute.
  useEffect(() => {
    if (!connected || range === "live") return;
    let live = true;
    setHistory(null);
    const load = async () => {
      try {
        const h = await invoke<HostHistory>("host_history", { host: host.name, range });
        if (live) setHistory(h);
      } catch {
        if (live) setHistory({ resolution_secs: 0, points: [] });
      }
    };
    void load();
    const t = setInterval(load, 60000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [host.name, connected, range]);

  async function install() {
    setBusy(`Installing otterd ${version} on ${host.name}…`);
    setError(null);
    try {
      setOutput(await invoke<string>("host_install", { name: host.name }));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(null);
    }
  }

  async function remove() {
    try {
      await invoke("host_remove", { name: host.name });
      onRemoved();
    } catch (e) {
      setError(String(e));
    }
  }

  const mine = forwards.filter((f) => f.host === host.name);
  // Older otterd doesn't know host.metrics: say what fixes it.
  const unsupported = metricsError !== null && /unknown variant|invalid request|invalid_request/i.test(metricsError);

  return (
    <main className="pane host-page">
      <div className="pane-head">
        <Menu
          label="Host actions"
          items={[{ label: "Remove host…", danger: true, onSelect: () => setConfirmRemove(true) }]}
        />
        <div className="pane-title">
          <h1>{host.name}</h1>
          <div className="chips">
            <span className="chip mono">{host.describe}</span>
            <span className="chip">{STATUS[host.status]}</span>
            {host.version && <span className="chip mono">otterd {host.version}</span>}
            {metrics && <span className="muted">up {uptime(metrics.uptime_secs)} · {metrics.cpus} CPUs</span>}
          </div>
        </div>
      </div>

      <div className="host-body">
        {host.message && !connected && <div className="notice">{host.message}</div>}
        {(offerInstall || busy || output || error) && (
          <div className="notice notice-row">
            <span>
              {busy ??
                (host.status === "not_installed"
                  ? `otterd isn’t installed on ${host.name}.`
                  : outdated
                    ? `${host.name} runs otterd ${host.version}; this app is ${version}.`
                    : offerInstall
                      ? `${host.name} runs an otterd this app can’t talk to.`
                      : "")}
              {error && <span className="error-text"> {error}</span>}
            </span>
            {offerInstall && (
              <button className="btn primary" disabled={!!busy} onClick={() => void install()}>
                {host.status === "not_installed" ? `Install otterd ${version}` : `Update otterd to ${version}`}
              </button>
            )}
          </div>
        )}
        {output && <pre className="output">{output}</pre>}

        <section aria-labelledby="usage">
          <div className="section-row">
            <h2 id="usage" className="section-title">Usage</h2>
            {connected && metrics && (
              <div className="segmented" role="radiogroup" aria-label="Time range">
                {RANGES.map(([value, label]) => (
                  <label key={value} className={range === value ? "on" : ""}>
                    <input type="radio" name="range" checked={range === value} onChange={() => setRange(value)} />
                    {label}
                  </label>
                ))}
              </div>
            )}
          </div>
          {!connected ? (
            <p className="muted">Shown when {host.name} is connected.</p>
          ) : unsupported ? (
            <p className="muted">Update otterd on {host.name} to see its usage.</p>
          ) : !metrics ? (
            <p className="muted">{metricsError?.includes("measuring") || !metricsError ? "Measuring…" : metricsError}</p>
          ) : (
            <Usage m={metrics} range={range} history={history} />
          )}
        </section>

        <section aria-labelledby="ports">
          <div className="section-row">
            <h2 id="ports" className="section-title">Ports</h2>
            {ssh && connected && (
              <span className="section-actions">
                <button className="btn outline" onClick={() => setMapping({ direction: "to_local" })}>
                  Map a port to this Mac
                </button>
                <button className="btn outline" onClick={() => setMapping({ direction: "to_host" })}>
                  Map a Mac port to {host.name}
                </button>
              </span>
            )}
          </div>
          {mine.length > 0 && <Mappings host={host} forwards={mine} />}
          <Listening
            host={host}
            ports={ports}
            canMap={ssh && connected}
            mapped={(p) => mine.some((f) => f.direction === "to_local" && f.targetPort === p)}
            onMap={(port) => setMapping({ direction: "to_local", port })}
          />
        </section>
      </div>

      {mapping && <MapDialog host={host} initial={mapping} onClose={() => setMapping(null)} />}
      {confirmRemove && (
        <Dialog title={`Remove ${host.name}?`} onClose={() => setConfirmRemove(false)}>
          <p className="muted">
            Otter forgets this host. Nothing on it is touched: otterd and its sessions keep running, and adding it again
            brings everything back.
          </p>
          <div className="form-actions">
            <span className="spacer" />
            <button className="btn outline" onClick={() => setConfirmRemove(false)}>
              Keep
            </button>
            <button className="btn danger" onClick={() => void remove()}>
              Remove host
            </button>
          </div>
        </Dialog>
      )}
    </main>
  );
}

function Usage({ m, range, history }: { m: HostMetrics; range: Range; history: HostHistory | null }) {
  const h = m.history;
  const cpu = h.map((s) => s.cpu_percent);
  const mem = h.map((s) => s.memory_used);
  const last = h[h.length - 1];
  const memFrac = m.memory.total ? m.memory.used / m.memory.total : 0;
  const loadFrac = m.cpus ? m.load[0] / m.cpus : 0;
  return (
    <>
      <div className="tiles">
        <div className="tile">
          <span className="tile-label">CPU</span>
          <span className="tile-value">{percent(last?.cpu_percent ?? 0)}</span>
          <span className="tile-sub">of {m.cpus} CPUs</span>
          <Sparkline values={cpu} max={100} />
        </div>
        <div className="tile">
          <span className="tile-label">Load average</span>
          <span className="tile-value">{m.load[0].toFixed(2)}</span>
          <span className="tile-sub">
            {m.load[1].toFixed(2)} · {m.load[2].toFixed(2)} (5, 15 min) — {percent(loadFrac * 100)} of capacity
          </span>
          <Meter used={m.load[0]} total={m.cpus} label="Load per CPU" />
        </div>
        <div className="tile">
          <span className="tile-label">Memory</span>
          <span className="tile-value">{bytes(m.memory.used)}</span>
          <span className="tile-sub">
            of {bytes(m.memory.total)} · {percent(memFrac * 100)}
            {m.memory.swap_total > 0 && ` · swap ${bytes(m.memory.swap_used)}`}
          </span>
          <Meter used={m.memory.used} total={m.memory.total} label="Memory used" />
        </div>
        <div className="tile">
          <span className="tile-label">Network</span>
          <span className="tile-value">↓ {rate(last?.net_rx_bps ?? 0)}</span>
          <span className="tile-sub">↑ {rate(last?.net_tx_bps ?? 0)}</span>
          <Sparkline values={h.map((s) => s.net_rx_bps + s.net_tx_bps)} />
        </div>
      </div>

      {range === "live" ? (
        <div className="charts">
        <LineChart title="CPU" series={[{ name: "CPU", color: "var(--series-1)", values: cpu }]} interval={m.interval_secs} format={percent} max={100} />
        <LineChart
          title="Memory used"
          series={[{ name: "Memory", color: "var(--series-1)", values: mem }]}
          interval={m.interval_secs}
          format={bytes}
          max={m.memory.total}
          binary
        />
        <LineChart
          title="Network"
          series={[
            { name: "In", color: "var(--series-1)", values: h.map((s) => s.net_rx_bps) },
            { name: "Out", color: "var(--series-2)", values: h.map((s) => s.net_tx_bps) },
          ]}
          interval={m.interval_secs}
          format={rate}
          binary
        />
        <LineChart
          title="Disk I/O"
          series={[
            { name: "Read", color: "var(--series-1)", values: h.map((s) => s.disk_read_bps) },
            { name: "Write", color: "var(--series-2)", values: h.map((s) => s.disk_write_bps) },
          ]}
          interval={m.interval_secs}
          format={rate}
          binary
        />
      </div>
      ) : (
        <Trends range={range} history={history} total={m.memory.total} />
      )}

      <div className="host-grid">
        <div>
          <h3 className="sub-title">Disks</h3>
          {m.disks.map((d) => (
            <div key={d.mount} className="disk">
              <div className="disk-line">
                <span className="mono">{d.mount}</span>
                <span className="muted">
                  {bytes(d.total - d.available)} of {bytes(d.total)} · {bytes(d.available)} free
                </span>
              </div>
              <Meter used={d.total - d.available} total={d.total} label={`${d.mount} used`} />
            </div>
          ))}
        </div>
        <div>
          <h3 className="sub-title">Busiest processes</h3>
          <table className="table">
            <thead>
              <tr>
                <th>Process</th>
                <th className="num">CPU</th>
                <th className="num">Memory</th>
              </tr>
            </thead>
            <tbody>
              {m.top.map((p) => (
                <tr key={p.pid}>
                  <td>
                    {p.name} <span className="muted mono">{p.pid}</span>
                  </td>
                  <td className="num">{percent(p.cpu_percent)}</td>
                  <td className="num">{bytes(p.memory)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </>
  );
}

function Mappings({ host, forwards }: { host: HostView; forwards: ForwardView[] }) {
  const [error, setError] = useState<string | null>(null);
  const call = async (cmd: string, args: Record<string, unknown>) => {
    setError(null);
    try {
      await invoke(cmd, args);
    } catch (e) {
      setError(String(e));
    }
  };
  return (
    <>
      <h3 className="sub-title">Mapped</h3>
      <table className="table">
        <thead>
          <tr>
            <th>On this Mac</th>
            <th aria-label="Direction" />
            <th>On {host.name}</th>
            <th>State</th>
            <th className="num">Keep</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {forwards.map((f) => {
            const toLocal = f.direction === "to_local";
            const here = toLocal ? `127.0.0.1:${f.listenPort}` : `${f.targetHost}:${f.targetPort}`;
            const there = toLocal ? `${f.targetHost}:${f.targetPort}` : `127.0.0.1:${f.listenPort}`;
            const key = { host: f.host, direction: f.direction, listenPort: f.listenPort };
            return (
              <tr key={`${f.direction}:${f.listenPort}`}>
                <td className="mono">
                  {toLocal ? (
                    <a href={`http://${here}`} onClick={(e) => { e.preventDefault(); void openUrl(`http://${here}`); }}>
                      {here}
                    </a>
                  ) : (
                    here
                  )}
                </td>
                <td aria-label={toLocal ? `from ${host.name} to this Mac` : `from this Mac to ${host.name}`} className="arrow">
                  {toLocal ? "←" : "→"}
                </td>
                <td className="mono">{there}</td>
                <td>
                  {f.state === "active" ? "Active" : f.state === "pending" ? "Waiting for connection" : <span className="error-text">{f.message}</span>}
                </td>
                <td className="num">
                  <input
                    type="checkbox"
                    aria-label="Keep across restarts"
                    checked={f.pinned}
                    onChange={(e) => void call("forward_pin", { ...key, pinned: e.target.checked })}
                  />
                </td>
                <td className="num">
                  <button className="btn outline small-btn" onClick={() => void call("forward_remove", key)}>
                    Remove
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      {error && <div className="notice error">{error}</div>}
    </>
  );
}

function Listening({
  host,
  ports,
  canMap,
  mapped,
  onMap,
}: {
  host: HostView;
  ports: ListeningPort[] | null;
  canMap: boolean;
  mapped: (port: number) => boolean;
  onMap: (port: number) => void;
}) {
  if (host.status !== "connected") return null;
  if (ports === null) return <p className="muted">Listening ports show once otterd on {host.name} is updated.</p>;
  return (
    <>
      <h3 className="sub-title">Listening on {host.name}</h3>
      {ports.length === 0 ? (
        <p className="muted">Nothing is listening.</p>
      ) : (
        <table className="table">
          <thead>
            <tr>
              <th className="num">Port</th>
              <th>Address</th>
              <th>Process</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {ports.map((p) => (
              <tr key={p.port}>
                <td className="num mono">{p.port}</td>
                <td className="mono muted">{p.address}</td>
                <td>{p.process ?? <span className="muted">—</span>}</td>
                <td className="num">
                  {canMap &&
                    (mapped(p.port) ? (
                      <span className="muted">mapped</span>
                    ) : (
                      <button className="btn outline small-btn" onClick={() => onMap(p.port)}>
                        Map to this Mac
                      </button>
                    ))}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  );
}

function MapDialog({
  host,
  initial,
  onClose,
}: {
  host: HostView;
  initial: { direction: Direction; port?: number };
  onClose: () => void;
}) {
  const toLocal = initial.direction === "to_local";
  const [remote, setRemote] = useState(initial.port ? String(initial.port) : "");
  const [local, setLocal] = useState(initial.port ? String(initial.port) : "");
  const [target, setTarget] = useState("");
  const [pinned, setPinned] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: FormEvent) {
    e.preventDefault();
    const r = Number(remote);
    const l = Number(local || remote);
    if (!r || !l) return;
    setBusy(true);
    setError(null);
    try {
      await invoke("forward_add", {
        host: host.name,
        direction: initial.direction,
        // The listening side: here for to_local, the host for to_host.
        listenPort: toLocal ? l : r,
        targetHost: target || null,
        targetPort: toLocal ? r : l,
        pinned,
      });
      onClose();
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  }

  return (
    <Dialog title={toLocal ? `Map a ${host.name} port to this Mac` : `Map a Mac port to ${host.name}`} onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <div className="row-2">
          <label className="field">
            <span>Port on {toLocal ? (target ? target : host.name) : host.name}</span>
            <input value={remote} onChange={(e) => setRemote(e.target.value.replace(/\D/g, ""))} placeholder="5173" inputMode="numeric" required />
          </label>
          <label className="field">
            <span>Port on this Mac</span>
            <input value={local} onChange={(e) => setLocal(e.target.value.replace(/\D/g, ""))} placeholder={remote || "same"} inputMode="numeric" />
          </label>
        </div>
        <details className="advanced">
          <summary>Advanced</summary>
          <label className="field">
            <span>{toLocal ? `Machine ${host.name} connects to` : "Machine this Mac connects to"}</span>
            <input value={target} onChange={(e) => setTarget(e.target.value)} placeholder="localhost (e.g. a database host)" spellCheck={false} />
          </label>
        </details>
        <label className="check">
          <input type="checkbox" checked={pinned} onChange={(e) => setPinned(e.target.checked)} />
          Keep across restarts
        </label>
        <p className="muted small">
          {toLocal
            ? `Opens on this Mac at 127.0.0.1:${local || remote || "…"}, carried by the ${host.name} SSH connection.`
            : `Reachable on ${host.name} at 127.0.0.1:${remote || "…"}.`}
        </p>
        {error && <div className="notice error">{error}</div>}
        <div className="form-actions">
          <span className="spacer" />
          <button type="button" className="btn outline" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="btn primary" disabled={busy || !remote}>
            {busy ? "Mapping…" : "Map"}
          </button>
        </div>
      </form>
    </Dialog>
  );
}

/** Usage over a recorded range: averages, with peaks for CPU and memory. */
function Trends({
  range,
  history,
  total,
}: {
  range: Exclude<Range, "live">;
  history: HostHistory | null;
  total: number;
}) {
  if (!history) return <p className="muted">Loading…</p>;
  const pts = history.points;
  if (pts.length === 0) {
    return (
      <p className="muted">
        Nothing recorded for this range yet. otterd records usage every minute while it runs (this needs otterd 0.3.1
        or later on the host), so trends fill in from now on.
      </p>
    );
  }
  const to = Date.now();
  const from = to - SPAN_MS[range];
  const at = pts.map((p) => Date.parse(p.at));
  const day = range === "7d" || range === "30d";
  const time = {
    at,
    from,
    to,
    // Join points at most two periods apart; a longer stretch wasn't recorded.
    gap: history.resolution_secs * 2000 + 1,
    label: (t: number) =>
      new Date(t).toLocaleString(undefined, day ? { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" } : { hour: "2-digit", minute: "2-digit" }),
  };
  return (
    <div className="charts">
      <LineChart
        title="CPU"
        series={[
          { name: "Average", color: "var(--series-1)", values: pts.map((p) => p.cpu_avg) },
          { name: "Peak", color: "var(--series-2)", values: pts.map((p) => p.cpu_max) },
        ]}
        interval={history.resolution_secs}
        format={percent}
        max={100}
        time={time}
      />
      <LineChart
        title="Memory used"
        series={[
          { name: "Average", color: "var(--series-1)", values: pts.map((p) => p.memory_avg) },
          { name: "Peak", color: "var(--series-2)", values: pts.map((p) => p.memory_max) },
        ]}
        interval={history.resolution_secs}
        format={bytes}
        max={total}
        binary
        time={time}
      />
      <LineChart
        title="Network"
        series={[
          { name: "In", color: "var(--series-1)", values: pts.map((p) => p.net_rx_bps) },
          { name: "Out", color: "var(--series-2)", values: pts.map((p) => p.net_tx_bps) },
        ]}
        interval={history.resolution_secs}
        format={rate}
        binary
        time={time}
      />
      <LineChart
        title="Disk I/O"
        series={[
          { name: "Read", color: "var(--series-1)", values: pts.map((p) => p.disk_read_bps) },
          { name: "Write", color: "var(--series-2)", values: pts.map((p) => p.disk_write_bps) },
        ]}
        interval={history.resolution_secs}
        format={rate}
        binary
        time={time}
      />
    </div>
  );
}
