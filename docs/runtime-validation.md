# Structured Claude runtime — validation record

What has been checked against real Claude, and what hasn't yet, for the `sdk`
backend (D-055–D-060). Each entry says how it was run and what was seen. No
keys or transcripts are kept here. "Pending" means not yet run, not failed.

## Version tuple

| | |
|---|---|
| Otter | 0.6.16 (`main` at the merge of #49) |
| Worker | 0.1.0, protocol 1 |
| SDK | `@anthropic-ai/claude-agent-sdk` 0.3.285, bundling Claude Code 2.1.285 |
| Node | 22.23.3 (macOS), 22.23.2 (Linux) |
| Hosts | macOS 27.0.1 arm64; Ubuntu 24.04 x86_64 (glibc 2.39) |
| Auth | the host's Claude Code sign-in (no API key) |
| Model | `haiku` for the daemon-level live tests; the SDK default for the probes |

## How

- **Worker probes:** `node packages/claude-runtime/probes/live.mjs`, on both
  hosts. 16 checks; see the worker's README for what each covers.
- **Daemon live tests:** isolated `OTTER_HOME` per test, a disposable
  workspace, the `rules` controller (no Otter model).
  `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime cargo test -p otterd
  --test e2e live_sdk -- --ignored --test-threads=1`. Last run 2026-10-10,
  macOS: `live_sdk_restart` and `live_sdk_controls` passed (59 s).
- **Packaging (D-060):** release v0.6.18 is the first with the worker
  (about 104–114 MB per platform, each with its `.sha256`).
  - `otter host install` from that release, into a throwaway `HOME`:
    - **macOS arm64 (local):** otterd 0.6.18 and the worker were installed;
      `runtime status` on the `sdk` backend said ready; a second install
      found the worker already there.
    - **Ubuntu x86_64 over SSH:** the same; the installed worker's `--check`
      passed (Node 22.23.2, its Claude Code present).
  - Installing 0.6.17, which has no worker, gave a note and a working
    otterd, reported "not ready".
  - The hosts' real installs weren't touched.

## The acceptance script (plan M6)

| # | Scenario | Status | Evidence |
|---|---|---|---|
| 1 | Non-Git coding task | Partial | `live_sdk_restart`: a disposable directory workspace; Claude ran a command and wrote the file it was asked for, and the feature reached review. Not yet tried: planning by a model and real tests. |
| 2 | Existing directory with `.envrc`/flake | Pending | — |
| 3 | Native context across runs | Passed | Probes on both hosts: a new run resumed the native session by id and recalled a fact. `live_sdk_restart` and `live_sdk_controls`: generation 2 continued the same native session. |
| 4 | Real interaction (permission, deny, form) | Passed at worker level | Probes: policy "ask" reached a permission request; a denied command didn't run; a two-question form was answered per question. Not yet tried: the same through the desktop app on a real feature. |
| 5 | Mid-run guidance | Partial | `live_sdk_controls`: a redirect mid-tool ran next in the same run, once, and the feature reached review. Not yet tried: queue one message *then* redirect, checking order. |
| 6 | Connectivity (app/SSH drop, reconnect) | Pending | Reconnect is covered offline (CLI and daemon e2e); not with real Claude over SSH. |
| 7 | Daemon crash mid-turn | Partial | `live_sdk_restart`: otterd **stopped** (graceful shutdown) mid-tool. The turn became `outcome_unknown`, its input `delivered`, and the next run resumed it as generation 2. Not yet tried: `SIGKILL` of otterd with real Claude (the leftover process-group cleanup is tested offline only). |
| 8 | Missing binding / auth / runtime | Partial | Missing worker, old Node and a worker that can't be checked are reported (`runtime status`; unit tests). Not yet tried: a missing sign-in or transcript with real Claude. |
| 9 | Limits | Pending | — |
| 10 | Git review | Pending | Needs an authorized disposable repository; publishing a PR needs explicit authorization. |

## Known limits

- A daemon stop ends the managed run. The next run resumes the native
  session, so what was said survives but the process doesn't (invariant 4's
  exception).
- No exactly-once guarantee for tool effects: a tool cut off by a crash is
  `result_unavailable`.
- Linux needs glibc; musl hosts aren't supported.
- Subagents (`Task`, `Agent`) are disallowed.
- Structured attachments are off; screenshot paste works in interactive
  sessions.
