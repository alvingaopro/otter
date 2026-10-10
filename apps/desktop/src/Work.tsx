import { useState } from "react";
import { Glyph } from "./Glyph";
import { ago } from "./model";
import {
  TOOL_GLYPH,
  TOOL_LABEL,
  conversationStatus,
  formAnswers,
  messageText,
  turnGlyph,
  turnLabel,
  turnText,
  type CodingConversation,
  type Interaction,
} from "./conversation";

/**
 * The coding agent's work on a feature, turn by turn (D-055…D-059): what it
 * was asked, what it said, the tools it used and how each went, what it
 * asked. Tool results aren't the agent's words; a turn completing isn't the
 * feature being done — the feature's checks and your review decide that.
 */
export function Work({ conversations, now }: { conversations: CodingConversation[]; now: number }) {
  if (conversations.length === 0) {
    return <p className="muted small">No coding work yet: it starts when a task runs.</p>;
  }
  return (
    <div className="work">
      {conversations.map((c) => (
        <section key={c.id} className="work-conversation" aria-label={`Conversation ${c.id}`}>
          <div className="work-head">
            <span className="work-status">{conversationStatus(c)}</span>
            <span className="spacer" />
            <span className="muted small" title={`${c.id} · ${c.backend}`}>
              {c.model ?? "default model"} · run {c.generation}
              {c.resumable ? "" : " · not resumable"}
            </span>
          </div>
          {c.read_only && (
            <p className="notice small" role="note">
              This conversation's record is read-only ({c.read_only}); what was recorded is below.
            </p>
          )}
          <ol className="turns">
            {c.turns.map((t) => {
              const said = c.messages.filter((m) => m.turn_id === t.id);
              const tools = c.tools.filter((x) => x.turn_id === t.id);
              const asked = c.interactions.filter((i) => i.turn_id === t.id);
              return (
                <li key={t.id} className={`turn ${t.state}`}>
                  <div className="turn-head">
                    <Glyph kind={turnGlyph(t)} />
                    <span className="turn-label">{turnLabel(t)}</span>
                    {t.redirect && <span className="chip small">change of direction</span>}
                    <span className="spacer" />
                    <span className="muted small">{ago(t.started_at ?? t.queued_at, now)}</span>
                  </div>
                  <div className="turn-input muted small">
                    {t.initiator === "user" ? "You" : "Otter"}: {firstLine(turnText(t))}
                  </div>
                  {t.reason && t.state === "finished" && t.outcome !== "completed" && (
                    <div className="turn-reason small">{t.reason}</div>
                  )}
                  {tools.length > 0 && (
                    <ul className="plain-list tools">
                      {tools.map((x) => (
                        <li key={x.id} className={`tool ${x.status}`}>
                          <details>
                            <summary>
                              <Glyph kind={TOOL_GLYPH[x.status]} />
                              <span className="tool-name">{x.name}</span>
                              <span className="tool-summary">{x.summary}</span>
                              <span className="muted small">{TOOL_LABEL[x.status]}</span>
                            </summary>
                            {x.result ? (
                              <pre className="tool-output">
                                {x.result.summary}
                                {x.result.truncated && "\n…"}
                              </pre>
                            ) : (
                              <p className="muted small">
                                {x.status === "result_unavailable" ? "Its result was never reported." : "No result yet."}
                              </p>
                            )}
                          </details>
                        </li>
                      ))}
                    </ul>
                  )}
                  {asked.map((i) => (
                    <div key={i.id} className={`asked ${i.status}`}>
                      <span className="muted small">
                        Asked: {i.summary ?? i.questions?.map((q) => q.prompt).join(" · ") ?? i.plan ?? i.type} — {i.status.replace("_", " ")}
                        {i.decided_by && ` (${i.decided_by === "user" ? "you" : i.decided_by})`}
                      </span>
                    </div>
                  ))}
                  {said.map((m) => (
                    <div key={m.id} className={`turn-said ${m.lifecycle}`}>
                      {messageText(m)}
                      {m.lifecycle === "interrupted" && <span className="muted small"> — cut off</span>}
                    </div>
                  ))}
                </li>
              );
            })}
          </ol>
        </section>
      ))}
    </div>
  );
}

const firstLine = (s: string) => {
  const line = s.split("\n").find((l) => l.trim()) ?? "";
  return line.length > 160 ? `${line.slice(0, 159)}…` : line;
};

/**
 * Every question of a form the coding agent asks, each with its own answer
 * (options, several where it allows, or your own words).
 */
export function QuestionForm({
  interaction,
  onAnswer,
}: {
  interaction: Interaction;
  onAnswer: (answers: Record<string, string>) => void;
}) {
  const questions = interaction.questions ?? [];
  const [chosen, setChosen] = useState<Record<string, string[]>>({});
  const [other, setOther] = useState<Record<string, string>>({});
  const answers = formAnswers(questions, chosen, other);
  const pick = (q: string, label: string, many: boolean) =>
    setChosen((c) => {
      const now = c[q] ?? [];
      const next = many ? (now.includes(label) ? now.filter((l) => l !== label) : [...now, label]) : [label];
      return { ...c, [q]: next };
    });
  return (
    <form
      className="question-form"
      onSubmit={(e) => {
        e.preventDefault();
        if (answers) onAnswer(answers);
      }}
    >
      {questions.map((q) => (
        <fieldset key={q.id} className="question">
          <legend>
            {q.header && <span className="chip small">{q.header}</span>} {q.prompt}
          </legend>
          {(q.options ?? []).map((o) => (
            <label key={o.label} className="option" title={o.description}>
              <input
                type={q.multi_select ? "checkbox" : "radio"}
                name={`q-${interaction.id}-${q.id}`}
                checked={(chosen[q.id] ?? []).includes(o.label)}
                onChange={() => pick(q.id, o.label, !!q.multi_select)}
              />
              <span>{o.label}</span>
              {o.description && <span className="muted small"> — {o.description}</span>}
            </label>
          ))}
          <input
            aria-label={`Your own answer: ${q.prompt}`}
            placeholder={q.options?.length ? "Or your own answer" : "Your answer"}
            value={other[q.id] ?? ""}
            onChange={(e) => setOther((o) => ({ ...o, [q.id]: e.target.value }))}
          />
        </fieldset>
      ))}
      <button className="btn primary" type="submit" disabled={!answers}>
        Answer
      </button>
    </form>
  );
}
