import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { briefSummary } from "./BriefDialog";
import { PanelGrip, SIDEBAR, SIDE_PANEL, clampWidth, usePanelWidth, type Panel } from "./panelResize";

let root: Root | undefined;
let host: HTMLDivElement;

function Row({ panel }: { panel: Panel }) {
  const [width, setWidth] = usePanelWidth(panel);
  return (
    <div>
      <PanelGrip panel={panel} width={width} onResize={setWidth} label="Width" />
      <aside style={{ width }} />
    </div>
  );
}

const grip = () => host.querySelector<HTMLElement>('[role="separator"]')!;
const width = () => host.querySelector("aside")!.style.width;
const press = (key: string) =>
  act(() => {
    grip().dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
  });
const mount = async (panel: Panel) => {
  root = createRoot(host);
  await act(async () => root?.render(<Row panel={panel} />));
};

describe("panel widths", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    localStorage.clear();
    host = document.createElement("div");
    document.body.appendChild(host);
  });
  afterEach(() => {
    act(() => root?.unmount());
    root = undefined;
    host.remove();
  });

  it("stay between the panel's least and what leaves its neighbour room", () => {
    expect(clampWidth(SIDEBAR, 50, 1200)).toBe(180);
    expect(clampWidth(SIDEBAR, 500, 1200)).toBe(500);
    expect(clampWidth(SIDEBAR, 1000, 1200)).toBe(800);
    expect(clampWidth(SIDE_PANEL, 900, 1000)).toBe(680);
    // A narrow row: the least still holds.
    expect(clampWidth(SIDE_PANEL, 400, 300)).toBe(220);
    // Room not known yet: only the least.
    expect(clampWidth(SIDE_PANEL, 2000, 0)).toBe(2000);
  });

  it("the sidebar widens to the right, and is remembered", async () => {
    await mount(SIDEBAR);
    expect(width()).toBe(`${SIDEBAR.initial}px`);
    await press("ArrowRight");
    await press("ArrowRight");
    expect(width()).toBe(`${SIDEBAR.initial + 32}px`);
    expect(grip().getAttribute("aria-valuenow")).toBe(String(SIDEBAR.initial + 32));
    expect(grip().getAttribute("aria-orientation")).toBe("vertical");

    // Opened again: the same width.
    act(() => root?.unmount());
    await mount(SIDEBAR);
    expect(width()).toBe(`${SIDEBAR.initial + 32}px`);

    await act(async () => grip().dispatchEvent(new MouseEvent("dblclick", { bubbles: true })));
    expect(width()).toBe(`${SIDEBAR.initial}px`);
  });

  it("the Files panel widens to the left, on its own key", async () => {
    await mount(SIDE_PANEL);
    await press("ArrowLeft");
    expect(width()).toBe(`${SIDE_PANEL.initial + 16}px`);
    await press("ArrowRight");
    await press("ArrowRight");
    expect(width()).toBe(`${SIDE_PANEL.initial - 16}px`);
    expect(localStorage.getItem(SIDE_PANEL.key)).toBe(String(SIDE_PANEL.initial - 16));
    expect(localStorage.getItem(SIDEBAR.key)).toBeNull();
  });

  it("dragging the edge follows the pointer, within the room beside it", async () => {
    await mount(SIDE_PANEL);
    const g = grip();
    Object.defineProperty(g.parentElement!, "clientWidth", { value: 1000 });
    g.setPointerCapture = () => {};
    const at = (type: string, clientX: number) =>
      act(() => {
        g.dispatchEvent(new MouseEvent(type, { bubbles: true, clientX, button: 0 }));
      });

    await at("pointerdown", 500);
    expect(document.body.classList.contains("resizing-col")).toBe(true);
    await at("pointermove", 400); // to the left: wider
    expect(width()).toBe(`${SIDE_PANEL.initial + 100}px`);
    await at("pointermove", -500); // the terminal keeps its room
    expect(width()).toBe(`${1000 - SIDE_PANEL.rest}px`);
    await at("pointerup", -500);
    expect(document.body.classList.contains("resizing-col")).toBe(false);
    await at("pointermove", 900); // let go: no longer follows
    expect(width()).toBe(`${1000 - SIDE_PANEL.rest}px`);
  });

  it("the sidebar is dragged to the right to widen it", async () => {
    await mount(SIDEBAR);
    const g = grip();
    g.setPointerCapture = () => {};
    const at = (type: string, clientX: number) =>
      act(() => {
        g.dispatchEvent(new MouseEvent(type, { bubbles: true, clientX, button: 0 }));
      });
    await at("pointerdown", 280);
    await at("pointermove", 400);
    await at("pointerup", 400);
    expect(width()).toBe(`${SIDEBAR.initial + 120}px`);
  });

  it("a remembered width that went bad falls back to the usual one", async () => {
    localStorage.setItem(SIDEBAR.key, "nonsense");
    await mount(SIDEBAR);
    expect(width()).toBe(`${SIDEBAR.initial}px`);
  });
});

describe("the brief's line", () => {
  it("is the title, else the description: there is no goal", () => {
    expect(briefSummary({})).toEqual({ text: undefined, more: 0 });
    expect(briefSummary({ title: "GCP renewal", description: "Scheduled offers", decisions: ["v1"] })).toEqual({
      text: "GCP renewal",
      more: 2,
    });
    expect(briefSummary({ description: "Scheduled offers" })).toEqual({ text: "Scheduled offers", more: 0 });
    // An older otterd's goal is not shown.
    expect(briefSummary({ goal: "old" } as never)).toEqual({ text: undefined, more: 0 });
  });
});
