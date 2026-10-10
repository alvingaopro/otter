# Claude worker

otterd's host-local worker for the `sdk` coding backend (D-057). It drives
Claude through the official [Claude Agent SDK](https://code.claude.com/docs/en/agent-sdk/overview)
and speaks versioned JSONL with otterd on stdin/stdout. One process per run;
no network listener; nothing in the desktop app imports it.

```
otterd ──stdin: initialize, send_turn, interrupt, policy_reply,
                resolve_interaction, shutdown──▶ worker ──SDK──▶ Claude Code
       ◀──stdout: ready, session, delivered, text_delta, text,
                tool_started, tool_finished, policy_check,
                permission_request, turn_finished, fatal──
```

## Compatibility record

| | |
|---|---|
| Worker protocol | 1 (`src/protocol.ts`, `crates/daemon/src/agents/claude_sdk.rs`) |
| SDK | `@anthropic-ai/claude-agent-sdk` 0.3.285 (exact, locked) |
| Claude Code | 2.1.285, bundled by the SDK (per-platform package); never the one on PATH |
| Node | 22 (tested 22.23.3 on macOS arm64, 22.23.2 on Linux x86_64) |
| Auth | the host's Claude Code sign-in (`auth source: none` = no API key; the login is used) — the developer's choice for their own hosts, see D-057 |

Probed live on macOS arm64 and Linux x86_64 (`probes/live.mjs`, 16/16
each; `OTTER_WORKER_TRACE=1` traces how turns end): initialization,
CLAUDE.md read with `settingSources: ["project"]`, an edit after Otter's
policy allowed it, streamed then whole text with one id, tool call and
result sharing an id, session cost, a policy "ask" reaching a permission
request (a project allow-rule is ignored in an untrusted workspace anyway),
a denied command not running, a two-question form answered per question,
an interrupt settling as `interrupted` (`error_during_execution`,
`terminal_reason: aborted_streaming`), and a new run resuming the native
session by id and remembering what it was told.

Observed and relied on:

- The session id (`system/init`) arrives with the first turn, not at
  initialization.
- `PreToolUse` hooks run for every tool call; `allow`, `deny` and `ask`
  work as documented; `ask` reaches `canUseTool`.
- `AskUserQuestion` answers are `{question text: answer}`, multi-select
  comma-joined.
- An interrupt ends the turn as `error_during_execution` — or, during a
  tool call, as a `success` — with `terminal_reason: aborted_*`; a turn that
  finished before the interrupt landed says `completed`.
- An interrupt sent before the provider has the turn may be lost (the turn
  then completes normally).
- Claude Code ignores a project's permission allow-rules in a workspace
  that hasn't been trusted interactively.

## Develop

```sh
npm ci            # installs the SDK and its bundled Claude Code for this platform
npm test          # type-check against the pinned SDK + protocol/mapping/worker tests
npm run build     # dist/main.js
node probes/live.mjs [model]   # live check against real Claude; uses this host's sign-in
```

To have otterd use a development build: `OTTER_CODING_BACKEND=sdk` (or
Settings → coding backend `sdk`) and `OTTER_CLAUDE_WORKER=$PWD` in otterd's
environment.

Released hosts get it from `otter host install` (D-060): a per-platform
`otter-claude-runtime-<version>-<platform>.tar.gz`, checked against its
`.sha256`, in `~/.local/lib/otter/claude-runtime/<version>`. `node
dist/main.js --check` says whether it can run here. What has been validated
live is in [`docs/runtime-validation.md`](../../docs/runtime-validation.md).
