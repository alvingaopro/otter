import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Meter, bytes, fpsLevel, onPerfOverlay, perfOverlay, terminalTotal, type Sample } from "./perf";

/** This app's process, as `ps` sees it (src-tauri `perf.rs`). */
interface ProcessStats {
  rssBytes: number;
  cpuPercent: number;
}

/** The performance overlay (D-128), while Settings › Developer has it on. */
export function PerfOverlay() {
  const [on, setOn] = useState(perfOverlay);
  useEffect(() => onPerfOverlay(setOn), []);
  return on ? <Readout /> : null;
}

const heap = () => (performance as Performance & { memory?: { usedJSHeapSize: number } }).memory?.usedJSHeapSize;

function Readout() {
  const [sample, setSample] = useState<Sample | null>(null);
  const [app, setApp] = useState<ProcessStats | null>(null);
  useEffect(() => {
    const meter = new Meter(performance.now(), terminalTotal());
    // Counted every frame, the rest read and shown once a second: counting
    // the page's elements or drawing the readout every frame would cost what
    // it measures.
    let frame = requestAnimationFrame(function tick(now) {
      meter.frame(now);
      if (meter.due(now)) setSample(meter.sample(now, terminalTotal(), document.getElementsByTagName("*").length, heap()));
      frame = requestAnimationFrame(tick);
    });
    // The main thread's lateness: a 100 ms timer, against when it was due.
    // Not while hidden, nor the first tick back: WebKit slows a hidden
    // window's timers, and that isn't the main thread being late.
    let due: number | undefined;
    const timer = setInterval(() => {
      const now = performance.now();
      if (due !== undefined) meter.late(Math.max(0, now - due));
      due = document.hidden ? undefined : now + 100;
    }, 100);
    // A hidden window draws no frames: say nothing rather than 0 fps.
    const visibility = () => {
      due = undefined;
      if (document.hidden) setSample(null);
      else meter.resume(performance.now(), terminalTotal());
    };
    document.addEventListener("visibilitychange", visibility);
    const poll = () => {
      if (document.hidden) return;
      invoke<ProcessStats>("process_stats")
        .then((p) => setApp(typeof p?.rssBytes === "number" ? p : null))
        .catch(() => setApp(null));
    };
    poll();
    const process = setInterval(poll, 2000);
    return () => {
      cancelAnimationFrame(frame);
      clearInterval(timer);
      clearInterval(process);
      document.removeEventListener("visibilitychange", visibility);
    };
  }, []);

  return (
    <div className="perf-overlay" aria-hidden="true">
      {sample ? (
        <>
          <div>
            <span className={`perf-fps ${fpsLevel(sample.fps)}`}>{sample.fps} fps</span> · worst {sample.worstFrame} ms
          </div>
          <div>
            lag {sample.lag} ms · {sample.domNodes.toLocaleString("en-US")} nodes
          </div>
          <div>terminals {bytes(sample.terminal)}/s</div>
          {sample.heap !== undefined && <div>heap {bytes(sample.heap)}</div>}
        </>
      ) : (
        <div>— fps</div>
      )}
      {app && (
        <div>
          app {bytes(app.rssBytes)} · {Math.round(app.cpuPercent)}% cpu
        </div>
      )}
    </div>
  );
}
