import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Markdown } from "./Markdown";

vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

let root: Root;
let host: HTMLDivElement;

beforeEach(() => {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

const render = (text: string) => act(() => root.render(<Markdown className="message-text" text={text} />));

describe("Markdown", () => {
  it("renders what the coding agent writes: headings, lists, emphasis, code", () => {
    render("## How it works\n\n1. Open a PR.\n2. Label it **backport v3.159**.\n\nRun `gh workflow run`:\n\n```sh\ngh workflow run backport.yaml\n```");
    expect(host.querySelector("h2")?.textContent).toBe("How it works");
    expect([...host.querySelectorAll("ol > li")].map((li) => li.textContent)).toEqual([
      "Open a PR.",
      "Label it backport v3.159.",
    ]);
    expect(host.querySelector("strong")?.textContent).toBe("backport v3.159");
    expect(host.querySelector("p code")?.textContent).toBe("gh workflow run");
    expect(host.querySelector("pre code")?.textContent).toBe("gh workflow run backport.yaml\n");
  });

  it("renders tables", () => {
    render("| a | b |\n|---|---|\n| 1 | 2 |");
    expect([...host.querySelectorAll("td")].map((td) => td.textContent)).toEqual(["1", "2"]);
  });

  it("keeps raw HTML and images inert, and opens only web links", async () => {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    render('<img src="x" onerror="alert(1)"> ![logo](https://example.com/a.png) [docs](https://example.com) [local](file:///etc/passwd)');
    expect(host.querySelector("img")).toBeNull();
    expect(host.textContent).toContain("[logo]");
    const links = host.querySelectorAll("a");
    expect(links).toHaveLength(1);
    act(() => links[0]!.click());
    expect(openUrl).toHaveBeenCalledWith("https://example.com");
    expect(host.textContent).toContain("local");
  });
});
