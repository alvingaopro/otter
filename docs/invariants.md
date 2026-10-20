# Architectural invariants

These must hold after every change. Each lists where it is enforced and what
checks it, so a change that breaks one is caught by a test, a review, or the
quick check given. Background: [`architecture-lessons.md`](architecture-lessons.md)
§22 and [`design.md`](design.md) §34.

If a change *needs* to break an invariant, that's an architecture decision:
record it in [`decisions.md`](decisions.md) first.

---

### 1. A Workspace does not require Git.

- `WorkspaceSource` (`crates/core/src/model.rs`) is `Empty | Git | Directory`;
  `Workspace` itself has only a `root`. Git metadata lives in `GitSource`.
- Tests: `scratch_workspace_with_default_login_shell`,
  `existing_directory_workspace_is_attached_not_managed`.

### 2. A Workspace may contain multiple Sessions.

- `Workspace.sessions: Vec<Session>`; nothing assumes one session (or one
  agent) per workspace. Two agents in one workspace bind to different
  conversations (`claimed` in `reconcile.rs`).
- Tests: `failures_and_completions_raise_attention`,
  `daemon_restart_adopts_running_sessions_and_records_changes`,
  `discovers_rollout_by_cwd_and_start_time`.

### 3. A Session survives control-plane disconnection.

- Processes belong to the execution backend under the daemon, never to a
  client connection; an attach is only a viewer (`attach.rs`). Tearing an
  attach down must not type anything into the session (D-017: no
  `portable_pty` writer, whose drop sends EOF).
- Tests: `client_disconnect_does_not_affect_session`,
  `attach_round_trip_resize_and_detach`,
  `attach_is_disposable_across_daemon_restart`,
  `attach_ends_cleanly_when_the_session_restarts_or_is_deleted`,
  `attach_shows_existing_screen_and_survives_repeated_cycles`, and for a
  stream attach (D-118: detaching closes the control client's stdin)
  `stream_attach_brings_the_history_and_round_trips`,
  `stream_attach_ends_with_the_process_and_leaves_it_on_disconnect`; manually
  verified by freezing the remote sshd mid-attach (D-017). An attach is
  looking, not answering: it settles a finished turn, never an approval or
  a question (D-099) — `looking_at_a_prompt_does_not_answer_it`. So is a
  stream's terminal on screen, for as long as it is (D-137) —
  `a_turn_that_ends_on_screen_is_already_seen`,
  `a_command_that_completes_on_screen_is_already_seen`,
  `a_terminal_gone_without_a_word_is_no_longer_on_screen`. A client that
  stops reading is let go, and the session runs on —
  `an_attach_whose_client_stops_reading_is_let_go`.

### 4. The control plane does not own agent process lifetime.

- `otterd dial` starts the daemon detached (`setsid`); `daemon.shutdown` stops
  the daemon but not managed processes, and a new daemon adopts them
  (`reconcile`).
- Test: `daemon_restart_adopts_running_sessions_and_records_changes`.
- Exception (D-044): a task's *managed* agent run talks to `otterd` over
  its stdin/stdout, so it ends when the daemon does. Its conversation
  doesn't: it is journaled (D-058), the run's agent session id is kept, a
  new daemon records the run's end (outcome unknown, never "completed")
  and marks it interrupted (`recover_runs`), and the controller resumes it.
  A daemon stopping is not a run failing.
  A run whose agent process dies is likewise `interrupted` and resumed, or
  `recovery_required` when continuing isn't safe (D-061).
- Tests: `runs_survive_a_restart_and_follow_a_changing_goal`,
  `changes_are_journaled_and_replayed_after_a_crash`,
  `a_run_lost_mid_tool_resumes_without_claiming_the_step`,
  `a_run_that_keeps_getting_lost_waits_for_the_developer`.

### 5. Agent-provider details do not leak into generic Workspace logic.

- Workspace, session, reconciliation and attention code talks to agents only
  through `AgentProvider` (`crates/daemon/src/agents/mod.rs`): `detect`,
  `launch_argv`, `observe`. What a provider persists is opaque
  (`AgentInfo::provider_session_id`, `AgentInfo::provider_state`).
- Quick check — Codex knowledge should appear only under `agents/`, plus the
  default provider name in the CLI:

  ```sh
  grep -rn -i 'codex\|rollout' crates --include='*.rs' \
    | grep -v 'crates/daemon/src/agents/\|crates/daemon/tests/'
  ```

### 6. Codex-specific details do not define the generic Session model.

- `Session.agent: Option<AgentInfo>` holds provider id, opaque provider data,
  and generic `AgentState`. Launch flags, rollout/transcript files,
  transcript parsing and hooks live in each provider (`agents/codex.rs`,
  `agents/claude.rs`); generic code only sees `Observation` (state, opaque
  data, a `Blocker` kind) and the shared quiet-turn fallback `agents::settle`.
- Tests: `agents::codex::tests::*` and `agents::claude::tests::*` (providers),
  and the end-to-end agent tests, which drive everything through the generic
  API with a fake `codex` / `claude`.

### 7. tmux is an execution backend, not the Session abstraction.

- `ExecutionBackend` (`backend/mod.rs`); only `backend/tmux.rs` (and its
  control-mode client, `backend/tmux/control.rs`) knows tmux.
  Executions are tmux sessions named by Otter's own execution id, so no tmux
  identifier enters the domain model. tmux's own messages are filtered from
  attach output and captures.
- Quick check: `grep -rn tmux crates --include='*.rs'` outside `backend/`
  should only find comments, the composition root that picks the backend
  (`main.rs`), the daemon's file layout (`paths.rs`), and environment hygiene
  (`env.rs`).

### 8. SSH is a transport, not the Host abstraction.

- A host is a durable `HostEntry` in the control plane's registry
  (`crates/cli/src/config.rs`); `Transport` (`crates/client`) is how it is
  reached (`ssh … otterd dial`, or locally). The daemon knows nothing about SSH.

### 9. A remote Workspace's filesystem lives on its Host.

- The control plane never touches a workspace path: `--dir` and `--repo` are
  passed through and resolved/validated by the daemon on the host; paths are
  only displayed.
- Quick check: no `std::fs` use on workspace roots in `crates/cli`.

### 10. Git worktrees are an implementation of Git-backed provisioning, not the Workspace.

- `git.rs` (`GitManager`) owns the shared bare repository and worktrees; it is
  used only for `WorkspaceSource::Git`.
- Test: `git_workspaces_share_one_backing_repository`.

### 11. The daemon is authoritative for runtime state.

- Durable state is `state.json` + `events.jsonl`, owned by the daemon. Clients
  hold no runtime state; reconciliation corrects the record from what is
  actually running.

### 12. The UI is disposable and reconnectable.

- Any client can disconnect at any time; everything it shows comes from
  `state.snapshot` / `workspace.list` plus `events.subscribe`. A reconnecting
  client resumes from its last event `seq` (and the log id it came from) and
  never silently misses events (`cursor_expired` → reload a snapshot), also
  when the log was rotated or started over. `otter ls --watch` and
  `otter events --follow` reconnect to hosts that drop.
- Tests: `event_replay_resumes_after_disconnect_and_daemon_restart`,
  `snapshot_then_subscribe_has_no_gap_and_stale_cursors_are_rejected`,
  `rotated_cursors_resync_and_recent_ones_replay`,
  `a_log_that_started_over_is_detected_even_past_the_old_cursor`, and for the
  CLI `events_follow_resyncs_when_the_hosts_log_starts_over`.

### 13. Secrets stay out of what Otter keeps and shows.

- Environments reach a process only through `otterd internal-exec`'s 0600
  env file, removed once read — never a command line, never
  `events.jsonl`. API keys are write-only: stored 0600, answered as
  `set: bool`.
- Text from outside — what direnv, tmux, Git, the Claude worker, the
  coding agent and its tools print — passes `otter_core::redact` before it
  becomes Otter's: state, events, conversation journals, task
  documents and messages, otterd's log, responses (D-075). A repository
  URL is kept as given in its workspace's record (Git needs it) and shown
  without credentials.
- Tests: `scratch_workspace_with_default_login_shell` (no env on tmux's
  command line, the env file gone), `credentials_in_failures_stay_out_of_what_otter_keeps`,
  `a_failing_worker_says_why`, the `redact` unit tests,
  `settings_take_keys_write_only_and_keep_them` (keys), and
  `the_developer_reads_and_edits_otters_memory` (Otter's memory, D-113).
