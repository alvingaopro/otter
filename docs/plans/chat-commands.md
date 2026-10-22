# Slash commands in the chat boxes: design

**Status:** built in D-190 (2026-10-22), with the recommended answers to
§6 except one: no Chinese aliases. All three phases went in one change.
Where the build differs from this text, `docs/decisions.md` D-190 is right.
It differs here: the *Pass on* heading is *Tell*, and holds `/revise` and
`/remember` too, which are done by otterd through Otter's tools rather than
by `request_changes` and a write to the memory from the app, so that every
command that leaves a message in the conversation is done the same way.
`task.send`'s `command` is an optional field, so no protocol version bump.
**Baseline:** `main` at `8ab5289` (v0.7.81).
**Design page:** https://claude.ai/artifact/3sDw9Z11fYXJdmGwK6Scbd
(Chinese, with mockups).

> **Core decision:** typing `/` at the start of a chat box lists every
> command Otter has, with the ones that can't be used now dimmed and why.
> A command either acts at once (as a button does), opens something in the
> app, or goes as a message that otterd does with the tool Otter's turn
> would have chosen. Nothing is left to a model's reading of a command.

## 1. Why

The developer asked (2026-10-22) for `/` commands with completion in the
chat box, listing what is supported, so that what Otter can do is found by
typing. Until then each ability had its own place: Start, Resume, Retry and
Accept in the header; Pause and Stop & send beside the box; Cancel, Delete
and Memory in the ⋯ menu; asking the coding agent, watching CI and
reminders only by asking Otter in words and hoping its turn chose the tool.

## 2. Commands in a task's conversation

| Command | Does | Usable when (why not, as shown) | Done by |
|---|---|---|---|
| `/start` | starts a draft | draft ("Already started") | `task.act start` |
| `/resume` | goes on from the pause | paused ("Not paused") | `task.act resume` |
| `/retry` | tries again, fresh budget | failed ("Hasn't failed") | `task.act retry` |
| `/accept` | accepts the work; with a gate not passed, Enter twice | review ("Not in review yet") | `task.act accept` |
| `/pause` | stops the work until resumed | planning, implementing, verifying, blocked ("Nothing to pause") | `task.act pause` |
| `/stop <what next>` | stops the agent's turn, has it do this | a run at work and `redirect` | `task.act redirect` |
| `/workspace <name>` | sets where it works; completes from the host's | not over, no run at work ("Pause first: the agent is working") | `task.act set_workspace` |
| `/own-workspace` | a worktree of its own (D-080) | waiting for a held workspace | `task.act own_workspace` |
| `/unwatch <watch>` | stops a watch | a watch waiting ("Nothing is watched") | `task.act cancel_watch` |
| `/cancel` | cancels; Enter twice | not over ("Already over") | `task.act cancel` |
| `/ask <question>` | asks the coding agent; it only reads (D-091) | a run at work on the SDK runtime | `ask_agent` |
| `/tell <message>` | passes it on as written, after its turn | a run at work | `forward_to_agent` |
| `/revise <what changed>` | plans again from where the work is | started, not cancelled | `revise_task` |
| `/remember <note>` | a line in this task's memory (D-113) | not cancelled | `remember` |
| `/watch ci` | watches the pull request's CI (D-183, D-188) | a pull request ("No pull request yet") | `watch` kind ci |
| `/remind <30m> [what]` | looks again then | not cancelled | `watch` kind time |
| `/answer-only on\|off` | only an answer, or changes are fine (D-092) | not over | `set_answer_only` |
| `/details` `/activity` `/changes` | the details, at Overview, Activity or Changes | always; Changes with a workspace | the app |
| `/open` `/memory` `/delete` | the workspace, the Memory dialog, the delete dialog | a workspace; memory; nothing at work | the app |

The *Tell* commands need an otterd that takes `command`
(`runtime.capabilities.message_commands`); elsewhere they are dimmed,
"Update otterd on this host to use this". Deciding (approve, deny, answer)
is not a command: the decision's buttons are above the box, and several may
wait at once.

## 3. Commands in the empty Chat page's box

Conditions on the task it starts, shown as chips above it, the words left
in the box its request: `/in <workspace>` (any host's, `name@host` where a
name is on several; sets the host too), `/repo <repository>` (a workspace of
its own from it, D-179), `/scratch`, `/host <name>`, `/answer-only`,
`/draft` (the button says Create draft). Where it works is one of in, repo
or scratch: the latest. They are `task.create`'s fields; nothing new on the
wire.

## 4. The box

- Only a `/` at the very start opens the menu; Pinyin's `、` and `／` there
  are made `/` (not while composing). A slash inside a sentence or a path is
  text.
- ↑ ↓ (⌃N ⌃P) move, skipping dimmed rows; Tab completes; Enter runs a
  command that takes nothing, completes one that takes more, and picks a
  choice. Esc closes the menu and keeps the words; armed for a second
  Enter, Esc clears the box. Every Enter goes through
  `useComposition().enterSubmits`, so an input method's Enter never runs
  one.
- Once the box names a command, a line above it says what Enter does (or
  why it can't, or what is missing), and the send button takes the
  command's name. A command that can't be used now is not sent. What isn't
  a command is sent as a message.
- Acts and Show clear the box when done; a failure leaves the words and
  says why. A Tell command takes the box's pictures with it.

## 5. otterd

`task.send {…, command}`: the message is stored with `command` and the
line as written. In the task's step, before Otter's turn,
`Daemon::run_commands` does each command not done yet: a
`Gateway::restricted` with one call, the message (its words only) as the
developer's word, so the tool's checks, history records and once-only keys
are the tools' own. Otter's line about it is added under
`command:<message id>` (applied once by that key), worded for the
developer. `controller::unanswered` leaves commands out. With the
controller off, only commands run.

## 6. Questions answered

1. How Tell commands reach Otter: a `command` field otterd does, not text a
   model reads. **Taken.**
2. Commands that can't be used now: dimmed with why, not hidden. **Taken.**
3. Chinese aliases (`暂停` for `/pause`): **not taken** — English names only.
4. The empty page's conditions with the task box's commands: **taken**, one
   change.
5. `/cancel` asks for Enter twice: **taken**.
