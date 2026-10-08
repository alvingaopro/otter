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
  `attach_shows_existing_screen_and_survives_repeated_cycles`; manually
  verified by freezing the remote sshd mid-attach (D-017).

### 4. The control plane does not own agent process lifetime.

- `otterd dial` starts the daemon detached (`setsid`); `daemon.shutdown` stops
  the daemon but not managed processes, and a new daemon adopts them
  (`reconcile`).
- Test: `daemon_restart_adopts_running_sessions_and_records_changes`.

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

- `ExecutionBackend` (`backend/mod.rs`); only `backend/tmux.rs` knows tmux.
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
