import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { ComposerGrip, DEFAULT_HEIGHT, clampHeight, useComposerHeight } from "./composerResize";

let root: Root;
let host: HTMLDivElement;

function Box() {
  const [height, setHeight] = useComposerHeight();
  return (
    <>
      <ComposerGrip height={height} onResize={setHeight} />
      <textarea style={{ height }} />
    </>
  );
}

const grip = () => host.querySelector<HTMLElement>('[role="separator"]')!;
const box = () => host.querySelector("textarea")!;
const press = (key: string) =>
  act(() => {
    grip().dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
  });

describe("message box height", () => {
  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    localStorage.clear();
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
  });
  afterEach(() => {
    act(() => root.unmount());
    host.remove();
  });

  it("stays between a few lines and most of the window", () => {
    expect(clampHeight(10, 800)).toBe(84);
    expect(clampHeight(200, 800)).toBe(200);
    expect(clampHeight(2000, 800)).toBe(480);
  });

  it("resizes from its handle and is remembered", async () => {
    await act(async () => root.render(<Box />));
    expect(box().style.height).toBe(`${DEFAULT_HEIGHT}px`);
    await press("ArrowUp");
    await press("ArrowUp");
    expect(box().style.height).toBe(`${DEFAULT_HEIGHT + 32}px`);
    expect(grip().getAttribute("aria-valuenow")).toBe(String(DEFAULT_HEIGHT + 32));

    // Opened again: the same height.
    act(() => root.unmount());
    root = createRoot(host);
    await act(async () => root.render(<Box />));
    expect(box().style.height).toBe(`${DEFAULT_HEIGHT + 32}px`);

    await act(async () => grip().dispatchEvent(new MouseEvent("dblclick", { bubbles: true })));
    expect(box().style.height).toBe(`${DEFAULT_HEIGHT}px`);
  });

  it("opens at the least when one remembered from before is smaller (D-175)", async () => {
    localStorage.setItem("otter.composer-height", "56");
    await act(async () => root.render(<Box />));
    expect(box().style.height).toBe("84px");
  });
});
