import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { choicesFor, find, isProblem, matches, parse, slashed, type Command } from "./chatCommands";
import { isImeKey } from "./composing";

/** A row of the menu: a command, or a value for the word after one. */
interface Item {
  key: string;
  head: string;
  arg?: string;
  what: string;
  /** Why it can't be used now; dimmed and skipped. */
  why?: string;
  /** The heading it starts. */
  group?: string;
  pick: () => void;
  complete: () => void;
}

/**
 * Slash commands in a chat box (D-190): the menu above (or below) it, the
 * line that says what Enter does, and the keys. `run` does what a command
 * comes to; the box's text and what else it holds are the caller's.
 */
export function useSlash<C, E>({
  text,
  setText,
  commands,
  ctx,
  enterSubmits,
  run,
  below,
}: {
  text: string;
  setText: (t: string) => void;
  commands: Command<C, E>[];
  ctx: C;
  /** The box's own test for an Enter that isn't an input method's. */
  enterSubmits: (e: KeyboardEvent) => boolean;
  run: (effect: E, command: Command<C, E>, rest: string) => void;
  /** The menu opens under the box (it has room there, not above). */
  below?: boolean;
}) {
  const [dismissed, setDismissed] = useState(false);
  // A command that asks for Enter twice, and the box as it was the first time.
  const [armed, setArmed] = useState<{ name: string; text: string } | null>(null);
  const [at, setAt] = useState(0);
  const parsed = parse(text);
  const isCommand = parsed !== null;
  // Esc holds until the box is no longer a command.
  useEffect(() => {
    if (!isCommand) setDismissed(false);
  }, [isCommand]);
  const command = parsed ? find(commands, parsed.name) : undefined;
  const why = command ? command.available(ctx) : true;
  const choosing = !!(parsed?.named && command?.choices && why === true && !/\s/.test(parsed.rest));
  const mode: "commands" | "choices" | null = !parsed || dismissed ? null : !parsed.named ? "commands" : choosing ? "choices" : null;

  function runCommand(c: Command<C, E>, rest: string) {
    const line = rest ? `/${c.name} ${rest}` : `/${c.name}`;
    if (c.available(ctx) !== true) return;
    const effect = c.effect(rest, ctx);
    // What's missing, or the second Enter asked for, is said above the box.
    if (isProblem(effect)) {
      setText(line);
      setDismissed(true);
      return;
    }
    const confirm = c.confirm?.(ctx);
    if (confirm && !(armed?.name === c.name && armed.text === slashed(text))) {
      setText(line);
      setDismissed(true);
      setArmed({ name: c.name, text: line });
      return;
    }
    setArmed(null);
    run(effect, c, rest);
  }

  const takesMore = (c: Command<C, E>) => !!(c.arg || c.choices || c.words);
  let items: Item[] = [];
  if (mode === "commands") {
    const rows = matches(commands, parsed!.name, ctx);
    let last: string | undefined;
    items = rows.map(({ command: c, why }) => {
      const group = !parsed!.name && c.group !== last ? c.group : undefined;
      last = c.group;
      const complete = () => setText(takesMore(c) ? `/${c.name} ` : `/${c.name}`);
      return {
        key: c.name,
        head: `/${c.name}`,
        arg: c.arg,
        what: c.what,
        why,
        group,
        complete,
        pick: () => (takesMore(c) ? complete() : runCommand(c, "")),
      };
    });
  } else if (mode === "choices" && command) {
    items = choicesFor(command, parsed!.rest, ctx).map((choice) => ({
      key: choice.value,
      head: choice.label,
      what: choice.detail ?? "",
      complete: () => setText(`/${command.name} ${choice.value}${command.words ? " " : ""}`),
      pick: () => runCommand(command, choice.value),
    }));
  }
  const usable = items.flatMap((item, i) => (item.why ? [] : [i]));
  const signature = `${mode}:${items.map((i) => i.key).join(",")}`;
  // Back to the first usable row whenever the rows change.
  useEffect(() => setAt(usable[0] ?? 0), [signature]);
  const current = usable.includes(at) ? at : usable[0];

  function move(step: 1 | -1) {
    if (usable.length === 0) return;
    const i = usable.indexOf(current ?? usable[0]);
    setAt(usable[(i + step + usable.length) % usable.length]);
  }

  /** What Enter (or the send button) does with a command in the box: `true` when it was a command's. */
  function enter(): boolean {
    if (mode && current !== undefined && items[current] && !items[current].why) {
      items[current].pick();
      return true;
    }
    if (parsed && command) {
      runCommand(command, parsed.rest.trim());
      return true;
    }
    return false;
  }

  /** The box's keydown: `true` when the menu took it. */
  function onKeyDown(e: KeyboardEvent): boolean {
    if (isImeKey(e)) return false;
    const menuOpen = mode !== null && items.length > 0;
    if (menuOpen && (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n"))) {
      e.preventDefault();
      move(1);
      return true;
    }
    if (menuOpen && (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p"))) {
      e.preventDefault();
      move(-1);
      return true;
    }
    if (menuOpen && e.key === "Tab" && !e.shiftKey && current !== undefined && !items[current]?.why) {
      e.preventDefault();
      items[current].complete();
      return true;
    }
    if (mode !== null && e.key === "Escape") {
      e.preventDefault();
      setDismissed(true);
      return true;
    }
    // Asked to press Enter again: Esc keeps things as they are.
    if (e.key === "Escape" && armed && armed.text === slashed(text)) {
      e.preventDefault();
      setArmed(null);
      setText("");
      return true;
    }
    if (enterSubmits(e) && enter()) {
      e.preventDefault();
      return true;
    }
    return false;
  }

  const list = useRef<HTMLUListElement>(null);
  useEffect(() => {
    list.current?.querySelector<HTMLElement>(`[data-at="${current}"]`)?.scrollIntoView?.({ block: "nearest" });
  }, [current]);

  const menu: ReactNode =
    mode === null ? null : (
      <div className={below ? "slash-menu below" : "slash-menu"} aria-label={mode === "commands" ? "Commands" : `/${command?.name}`}>
        {items.length === 0 ? (
          <p className="slash-none">No command matches · Enter sends it as a message</p>
        ) : (
          <ul className="slash-list" role="listbox" ref={list}>
            {items.map((item, i) => (
              <li key={item.key} role="presentation">
                {item.group && <div className="slash-group">{item.group}</div>}
                <div
                  role="option"
                  aria-selected={i === current}
                  aria-disabled={item.why ? true : undefined}
                  data-at={i}
                  className={item.why ? "slash-row off" : i === current ? "slash-row on" : "slash-row"}
                  onMouseMove={() => !item.why && setAt(i)}
                  // The box keeps the focus.
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => !item.why && item.pick()}
                >
                  <span className="slash-cmd">
                    {item.head}
                    {item.arg && <span className="slash-arg"> {item.arg}</span>}
                  </span>
                  <span className="slash-what">{item.what}</span>
                  {item.why && <span className="slash-why">{item.why}</span>}
                </div>
              </li>
            ))}
          </ul>
        )}
        <div className="slash-foot" aria-hidden="true">
          <span>
            <kbd>↑</kbd>
            <kbd>↓</kbd> choose
          </span>
          <span>
            <kbd>Tab</kbd> complete
          </span>
          <span>
            <kbd>↵</kbd> {mode === "choices" ? "choose" : "run"}
          </span>
          <span>
            <kbd>esc</kbd> close
          </span>
        </div>
      </div>
    );

  // What Enter does, once the box names a command (and the menu isn't choosing one).
  let hint: { text: string; warn: boolean } | null = null;
  let button: { label: string; disabled: boolean } | null = null;
  if (parsed && command && mode !== "commands") {
    const rest = parsed.rest.trim();
    const effect = why === true ? command.effect(rest, ctx) : undefined;
    const problem = effect && isProblem(effect) ? effect.problem : undefined;
    const confirm = armed?.name === command.name && armed.text === slashed(text) ? command.confirm?.(ctx) : undefined;
    hint =
      why !== true
        ? { text: why, warn: true }
        : confirm
          ? { text: confirm, warn: true }
          : problem && rest
            ? { text: problem, warn: true }
            : { text: command.does(ctx), warn: false };
    button = { label: command.button, disabled: why !== true || !!problem };
  }
  const hintLine: ReactNode = hint && (
    <p className={hint.warn ? "slash-hint warn" : "slash-hint"} role="status">
      <span className="slash-chip">/{command!.name}</span>
      <span>{hint.text}</span>
    </p>
  );

  return { menu, hint: hintLine, button, onKeyDown, enter, command: parsed && command ? command : undefined };
}
