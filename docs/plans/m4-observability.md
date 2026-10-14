# M4 — Observability & diagnostics: implementation plan

**Milestone:** M4 (P1) of [`docs/roadmap.md`](../roadmap.md): *failures can
be diagnosed without exposing secrets or reading raw terminal output.*
**Written without the milestone's spec** (`04-…` isn't on this machine),
on the requester's instruction to go on, as M3 was (D-070): the scope is
derived from the exit outcome, the design's rules on secrets (§20, D-005),
`docs/invariants.md`, and an audit of the code (§2). When the spec arrives,
it wins (D-074).
**Status:** in progress — step 1 (D-075). **Branch:** `roadmap-m4`, from `main` after M3.

## 1. The outcome, read two ways

- **Diagnosable without raw terminal output:** when something fails — a
  workspace, a session, a run, a feature, the host itself — Otter says what
  failed, why, and what to do next, in the CLI and the app, without the
  developer attaching to a pane or reading scrollback (which is gone after
  a restart anyway).
- **Without exposing secrets:** what Otter keeps and shows about a failure
  — state, events, the conversation journal, feature messages, its own log,
  anything a developer might paste into an issue — never carries a
  credential, even when the failing tool printed one.

## 2. The code against that (audit)

Already met, kept as is: environments go through `otterd internal-exec`'s
0600 env file, removed after reading (e2e `env` test); commands and keys stay
out of `events.jsonl` (e2e); API keys are write-only, 0600, never echoed
(e2e); `gh` gets no token on its command line (e2e); git's stderr is
redacted (`git.rs` `redact_credentials`); a workspace that fails to prepare
says why in the CLI with the next step (`otter ws prepare`); a feature that
stops says why (`status_reason`); `otter runtime status` names what's
missing with how to fix it.

The gaps:

1. **Unredacted failure text reaches durable state.** direnv's stderr (into
   the workspace's `state_message`, `EnvironmentFailed` events, otterd's
   log), tmux's stderr (`launch_error`, `ExecutionFailed`), the Claude
   worker's stderr (a turn's reason → the run summary → `last_error` → a
   message in the feature), and a repository URL with a token in it (kept
   and printed as given). Only git's stderr is redacted. No invariant says
   secrets stay out of durable state.
2. **Why a process ended is only in its pane.** A session that exits
   non-zero raises "X failed (status N)"; an agent that crashes, can't sign
   in or hits a rate limit, and `internal-exec` failing to start a program,
   are only in tmux scrollback — released on restart, gone for a lost
   execution.
3. **Error categories without next steps.** A coding-agent turn's error
   (`auth_required`, `rate_limited`, `resume_unavailable`, …) has words only
   in the app, three with a next step; the CLI prints the bare category.
4. **A failed workspace in the app** shows "workspace preparation failed";
   the reason is only in the Timeline.
5. **No way to diagnose a host.** Startup problems — the login environment
   couldn't be captured, tmux missing, settings or conversations that
   couldn't load — only warn in otterd's log, which nothing shows and
   nothing rotates. `otter host status` lists tools; runtime readiness is a
   separate command; the app's host page shows neither.
6. **A feature's failure can't be traced from the terminal:** no CLI for a
   feature's history; `otter feature show` leaves out the task's last error.

## 3. Steps

Each lands as its own PR (`release:skip`) with tests; the release gate runs
on each (the desktop build through CI's Desktop job, on macOS).

### Step 1 — Secrets stay out of what Otter keeps (gap 1)

One redactor in `otter-core` (moved from `git.rs`, widened): credentials in
URLs, `Authorization:`/`Bearer` values, `key=value` pairs whose key names a
secret (`token`, `password`, `secret`, `api_key`, …), well-known token
shapes (GitHub, Anthropic, OpenAI, Slack, AWS access keys, Google API
keys), private-key blocks. Applied where outside text becomes Otter's:
direnv and tmux stderr, the worker's stderr and every turn reason it
feeds, git, a repository URL as stored and shown (the clone still gets the
URL as given — it's the developer's), otterd's own log lines for those. A
new invariant (13) with a test that feeds secret-shaped failures through
each path and searches state, events, journals and the log.

### Step 2 — Failures say why and what next (gaps 2, 3, 4, 6)

- When a session's process ends, otterd keeps its last lines (bounded,
  redacted) with the execution, before the pane goes; the attention item
  and the CLI/app say "X failed (status N): <last line>", and `otter ws
  show` / the app show the tail. A failed `internal-exec` says what it
  couldn't start.
- Each error category gets words and a next step in `otter-core`, used by
  the CLI and served to the app.
- The app shows a failed workspace's reason in its banner.
- `otter feature history <feature>`, and `otter feature show` prints the
  failing task's last error.

### Step 3 — Diagnose a host (gap 5)

`otter doctor [host]`: one report — the tools, the coding runtime's
readiness (with the worker's own complaint, redacted), the problems otterd
met at startup and since (recorded, not only logged), damaged
conversations, where otterd's log is and how big, whether Otter's model is
set up — each problem with what to do. Non-zero exit when something is
wrong. The app's host page shows the same. otterd's log rotates.

### Step 4 — Record

Decisions (D-075 onward), invariant 13 in `docs/invariants.md`, the live
scenarios (a real auth failure, a crashing agent, a direnv error with a
token in it) pending in `runtime-validation.md`.

## 4. Not in this plan

A hosted telemetry backend (out of scope in the roadmap), metrics and
traces export, a shareable diagnostics bundle — candidates for the spec.

## 5. Acceptance (derived)

- [x] A secret printed by a failing tool (direnv, tmux, the Claude worker,
  git, the coding agent's tools) or embedded in a repository URL appears in
  no event, journal, feature message, answer or kept file — the
  workspace's record keeps its URL as given (invariant 13's test).
- [ ] A process that fails says why without the pane: its last lines are
  kept with it and shown, also after otterd restarts.
- [ ] Every error category says what happened and what to do, in the CLI
  and the app.
- [ ] `otter doctor` lists every problem a host has, each with a next step.
