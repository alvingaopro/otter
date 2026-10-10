import { describe, expect, it } from "vitest";
import { conversationStatus, formAnswers, turnLabel, type CodingConversation, type CodingTurn } from "./conversation";

const turn = (over: Partial<CodingTurn>): CodingTurn => ({
  id: "turn_1",
  initiator: "controller",
  input: [{ type: "text", text: "do it" }],
  delivery: "delivered",
  state: "finished",
  queued_at: "2026-10-10T00:00:00Z",
  ...over,
});

const conv = (over: Partial<CodingConversation>): CodingConversation => ({
  id: "conv_1",
  provider: "claude",
  backend: "sdk",
  lifecycle: "open",
  generation: 1,
  turns: [],
  messages: [],
  tools: [],
  interactions: [],
  resumable: true,
  updated_at: "2026-10-10T00:00:00Z",
  ...over,
});

describe("the coding agent's state, in words", () => {
  it("says what a turn is doing or how it ended — never just 'failed'", () => {
    expect(turnLabel(turn({ state: "running" }))).toBe("Working");
    expect(turnLabel(turn({ state: "waiting" }))).toBe("Needs you");
    expect(turnLabel(turn({ state: "interrupting" }))).toBe("Stopping the current work");
    expect(turnLabel(turn({ state: "queued", delivery: "queued" }))).toBe("Queued after the current work");
    expect(turnLabel(turn({ state: "queued", redirect: true }))).toBe("Next: a change of direction");
    expect(turnLabel(turn({ outcome: "completed" }))).toBe("Done");
    expect(turnLabel(turn({ outcome: "interrupted" }))).toBe("Stopped");
    expect(turnLabel(turn({ outcome: "limit_reached" }))).toBe("Stopped at a limit");
    expect(turnLabel(turn({ outcome: "outcome_unknown" }))).toBe("Outcome needs checking");
    expect(turnLabel(turn({ outcome: "cancelled", delivery: "queued" }))).toBe("Not sent");
    // Why it failed, when it's known.
    expect(turnLabel(turn({ outcome: "failed", error: "auth_required" }))).toMatch(/signing in/);
    expect(turnLabel(turn({ outcome: "failed", error: "rate_limited" }))).toMatch(/Rate limited/);
  });

  it("sums up a conversation: what runs, else why not", () => {
    expect(conversationStatus(conv({ turns: [turn({ state: "running" })] }))).toBe("Working");
    expect(conversationStatus(conv({ lifecycle: "paused", turns: [turn({ outcome: "interrupted" })] }))).toBe("Paused");
    expect(conversationStatus(conv({ turns: [turn({ outcome: "interrupted" }), turn({ id: "t2", state: "queued", delivery: "queued" })] }))).toBe(
      "1 queued for the next run",
    );
    expect(conversationStatus(conv({ read_only: "disk full" }))).toMatch(/Read-only/);
    expect(conversationStatus(conv({}))).toBe("Not started");
  });

  it("answers every question of a form, each its own way", () => {
    const qs = [
      { id: "Which format?", prompt: "Which format?", options: [{ label: "CSV" }, { label: "JSON" }] },
      { id: "Which columns?", prompt: "Which columns?", options: [{ label: "name" }, { label: "time" }], multi_select: true },
    ];
    expect(formAnswers(qs, { "Which format?": ["CSV"] }, {})).toBeNull();
    expect(formAnswers(qs, { "Which format?": ["CSV"], "Which columns?": ["name", "time"] }, {})).toEqual({
      "Which format?": "CSV",
      "Which columns?": "name, time",
    });
    // Your own words win over a choice.
    expect(formAnswers(qs, { "Which format?": ["CSV"], "Which columns?": ["name"] }, { "Which format?": "YAML" })).toEqual({
      "Which format?": "YAML",
      "Which columns?": "name",
    });
  });
});
