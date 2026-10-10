import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CodingConversation, Interaction } from "./conversation";
import { QuestionForm, Work } from "./Work";

let root: Root;
let host: HTMLDivElement;

beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
});
afterEach(() => {
  act(() => root.unmount());
  host.remove();
});

const now = Date.parse("2026-10-10T00:10:00Z");
const at = "2026-10-10T00:00:00Z";

const conversation: CodingConversation = {
  id: "conv_1",
  provider: "claude",
  backend: "sdk",
  lifecycle: "open",
  generation: 2,
  model: "claude-sonnet-5-5",
  resumable: true,
  updated_at: at,
  turns: [
    { id: "t1", initiator: "controller", input: [{ type: "text", text: "Fix the failing test" }], delivery: "delivered", state: "finished", outcome: "interrupted", queued_at: at },
    { id: "t2", initiator: "user", input: [{ type: "text", text: "Use tabs" }], delivery: "delivered", state: "running", redirect: true, queued_at: at },
  ],
  messages: [
    { id: "m1", turn_id: "t1", role: "agent", blocks: [{ id: "b1", type: "text", text: "Looking at the tes" }], lifecycle: "interrupted", at },
  ],
  tools: [
    { id: "x1", turn_id: "t1", run_id: "r", name: "Bash", effect: "command", summary: "npm test", status: "failed", started_at: at, result: { summary: "1 failed" } },
    { id: "x2", turn_id: "t1", run_id: "r", name: "Edit", effect: "edit", summary: "src/a.ts", status: "result_unavailable", started_at: at },
    { id: "x3", turn_id: "t2", run_id: "r", name: "Read", effect: "read", summary: "src/b.ts", status: "running", started_at: at },
  ],
  interactions: [],
};

describe("Work", () => {
  it("shows each turn, its tools and how they went, without dressing results as the agent's words", async () => {
    await act(async () => root.render(<Work conversations={[conversation]} now={now} />));
    const text = host.textContent ?? "";
    expect(text).toContain("Working"); // what runs now
    expect(text).toContain("Stopped"); // the first turn, interrupted
    expect(text).toContain("change of direction");
    expect(text).toContain("claude-sonnet-5-5");
    // A failed tool, its output on demand; a result never reported is said so.
    const tools = [...host.querySelectorAll(".tool")];
    expect(tools.map((t) => t.className)).toEqual(["tool failed", "tool result_unavailable", "tool running"]);
    expect(tools[0].querySelector("pre")?.textContent).toContain("1 failed");
    expect(tools[1].textContent).toContain("never reported");
    // Text cut off by the interrupt is marked, not shown as finished.
    expect(host.querySelector(".turn-said.interrupted")?.textContent).toContain("cut off");
  });

  it("says when there's nothing yet", async () => {
    await act(async () => root.render(<Work conversations={[]} now={now} />));
    expect(host.textContent).toContain("No coding work yet");
  });
});

describe("QuestionForm", () => {
  const form: Interaction = {
    id: "dec_1",
    turn_id: "t1",
    generation: 1,
    type: "questions",
    status: "pending",
    requested_at: at,
    questions: [
      { id: "Which format?", prompt: "Which format?", header: "Format", options: [{ label: "CSV" }, { label: "JSON", description: "one per line" }] },
      { id: "Which columns?", prompt: "Which columns?", options: [{ label: "name" }, { label: "time" }], multi_select: true },
    ],
  };

  it("answers each question on its own, several where it allows", async () => {
    const onAnswer = vi.fn();
    await act(async () => root.render(<QuestionForm interaction={form} onAnswer={onAnswer} />));
    const submit = host.querySelector<HTMLButtonElement>('button[type="submit"]')!;
    expect(submit.disabled).toBe(true);
    const option = (label: string) =>
      [...host.querySelectorAll<HTMLLabelElement>("label.option")].find((l) => l.textContent?.startsWith(label))!.querySelector("input")!;
    expect(option("CSV").type).toBe("radio");
    expect(option("name").type).toBe("checkbox");
    await act(async () => option("JSON").click());
    expect(submit.disabled).toBe(true); // one question still open
    await act(async () => option("name").click());
    await act(async () => option("time").click());
    expect(submit.disabled).toBe(false);
    await act(async () => submit.click());
    expect(onAnswer).toHaveBeenCalledWith({ "Which format?": "JSON", "Which columns?": "name, time" });
  });
});
