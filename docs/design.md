# Otter V1 — Design & Architecture

> Status: V1 architecture. Some areas are deliberately left as **open decisions**
> (see §35) rather than over-designed — notably the exact Otter RPC protocol,
> the persistence format, and the Codex state-detection mechanism. The
> foundation can be built without those being perfect.
>
> Implementation decisions and deviations from this document are recorded in
> [`decisions.md`](decisions.md).

## 1. Overview

Otter is a control plane for managing persistent development work across remote
machines.

The primary problem is not terminal management. It is managing multiple
concurrent workspaces and knowing which work currently requires human attention.

A typical user may have several coding agents running simultaneously across
multiple remote hosts. Each workspace may contain an agent, shells, development
servers, tests, and other long-running processes.

The user should interact primarily with:

```
Workspaces → Sessions → Attention
```

rather than:

```
Hosts → SSH → tmux → panes
```

Infrastructure such as SSH, tmux, Git, Nix, direnv, and Codex are implementation
details or optional integrations.

## 2. V1 Goals

V1 should support:

- Multiple remote hosts.
- SSH as the only remote transport.
- A Otter daemon installed on every managed host.
- Persistent workspaces.
- Multiple sessions per workspace.
- Codex sessions.
- Interactive shell sessions.
- Long-running services such as development servers.
- Background tasks such as tests.
- tmux-backed process persistence.
- Optional Git repositories.
- Git worktrees for isolated Git-backed workspaces.
- Shared Git object storage to avoid cloning the repository per workspace.
- direnv-managed development environments.
- Repositories using `flake.nix` / Nix.
- Workspace lifecycle management.
- Session status.
- Basic event/timeline tracking.
- Human-attention state.
- Reconnection after the client disappears.

The system should be usable without Git. For example, this must be a valid
workspace:

```
Workspace: scratch
Sessions:
  Codex
  Shell
```

## 3. Non-Goals for V1

Do not implement:

- Kubernetes.
- Generic cluster scheduling.
- Docker orchestration.
- Workspace migration between hosts.
- Sessions distributed across multiple hosts.
- Multiple remote transport protocols.
- Built-in Git credential management.
- Built-in Codex credential management.
- Built-in package management.
- Full secret management.
- Multi-user collaboration.
- Automatic agent-to-agent orchestration.
- A replacement terminal multiplexer.
- A replacement for Nix or direnv.

Prefer the existing tools whenever possible.

## 4. Core Design Principle

A Workspace is a durable logical context for related work.

It is **not**:

- a Git repository;
- a Git worktree;
- a tmux session;
- a directory;
- a Codex conversation;
- a process.

Those may be resources used by a Workspace.

The core hierarchy for V1 is:

```
Control Plane
    │
    ├── Host
    │    │
    │    └── Workspace
    │          │
    │          ├── Brief
    │          ├── Resources
    │          └── Sessions
    │                │
    │                └── Execution
    │
    ├── Events
    │
    └── Attention
```

For V1, every Workspace executes entirely on one Host. This constraint may be
relaxed later.

## 5. Main Entities

### 5.1 Host

A Host is a machine capable of running Workspaces.

```yaml
id: dev-01
name: Development VM
connection:
  ssh_host: dev-01
capabilities:
  otterd: true
  git: true
  tmux: true
  nix: true
  direnv: true
  codex: true
```

SSH is the only transport supported by V1.

The user's existing SSH configuration and SSH agent should be reused whenever
possible. If `ssh dev-01` works, Otter should ideally be able to connect.

## 6. Connectivity

Each managed host runs `otterd`.

The daemon does not expose a network port. It listens on a **Unix domain
socket** owned by the user (`~/.otter/run/workd.sock`, mode `0600`), so that
other local users on a shared host cannot reach it. *(Amended during
implementation — the original draft used `localhost:<port>`; see
[decisions.md D-003](decisions.md).)*

```
Control Plane
      │
      │ SSH
      ▼
Remote Host
      │
      ├── otterd
      │    ~/.otter/run/workd.sock
      │
      └── workspace processes
```

The control plane reaches the socket through an SSH channel. In V1 each client
connection runs `ssh <host> otterd dial`, which bridges the channel's stdio to
the daemon socket (starting the daemon if it is not running). SSH connection
multiplexing (`ControlMaster`) keeps repeated connections cheap.

SSH provides:

- authentication;
- encryption;
- remote execution;
- tunneling;
- file transfer when needed.

Do not introduce another transport abstraction in V1.

## 7. Workspace

A Workspace groups related work.

```yaml
id: ws_01
name: Fix GCP renewal validation
host: dev-01
root: ~/.otter/workspaces/ws_01/repo
brief:
  goal: Fix scheduled offer renewal validation.
source:
  type: git
  repository: suger-api
  branch: otterd/ws_01
  base: origin/main
environment:
  type: direnv
sessions:
  - agent
  - shell
  - dev-server
```

A minimal Workspace may instead be:

```yaml
id: ws_02
name: scratch
host: dev-01
root: ~/.otter/workspaces/ws_02
```

Git is completely optional.

## 8. Workspace Brief

The Brief represents durable human-level information describing why the
Workspace exists.

```yaml
title: Fix GCP renewal validation
goal: >
  Fix validation behavior for scheduled GCP Marketplace
  offers.
description: >
  Scheduled custom offers require duration while
  on-acceptance custom offers require an end date.
constraints:
  - Preserve Salesforce behavior.
  - Maintain backwards compatibility.
references:
  - Linear SUG-3821
decisions:
  - Renewal count includes the initial term.
```

Every field other than the workspace identity should be optional.

The brief is set at creation (`otter new --goal`, the app's New workspace
goal field) and edited any time, in any state, with `workspace.set_brief`
(`otter ws brief`, the app's brief line under the workspace name): D-038.

The Brief should not be confused with an agent's internal conversation context.

## 9. Agent Context

Agent-specific state belongs to the agent integration.

```yaml
provider: codex
resume_id: abc123
```

Otter should not attempt to recreate or understand Codex's complete internal
context. If Codex supports resuming a session, Otter should retain whatever
opaque identifier is required.

```
Workspace
    │
    ├── Brief
    │
    └── Agent Session
            │
            └── provider-owned context
```

## 10. Session

A Session is a durable logical activity inside a Workspace.

Initial session kinds:

| Kind       | Example         |
|------------|-----------------|
| `agent`    | Codex           |
| `terminal` | Shell           |
| `service`  | `npm run dev`   |
| `task`     | `npm test`      |

A Workspace can contain multiple Sessions. Do not assume one Workspace equals
one coding agent.

## 11. Session vs Execution

Session identity must be separated from process identity.

```
Session
  id: session_123
  kind: agent
  provider: codex

Execution
  id: exec_001
  host: dev-01
  backend: tmux
  pid: 38491
```

If the process dies and is restarted:

```
session_123
    exec_001 [dead]
    exec_002 [running]
```

The logical Session remains the same. This distinction enables future recovery
and restart behavior.

## 12. tmux

tmux is the V1 execution/persistence backend. Otter should use tmux rather than
attempting to implement terminal persistence itself.

```
Session
    │
    ▼
Execution
    │
    ▼
Tmux Backend
    │
    ▼
PTY / Process
```

tmux is an implementation detail. Do not expose tmux concepts as fundamental
concepts in the API or data model. The architecture should make another
execution backend possible in the future.

## 13. Git Repository Management

Git is an optional Workspace resource.

Repositories should not be cloned separately for every Workspace. Each host
maintains shared backing repositories.

Suggested filesystem layout:

```
~/.otter/
    repos/
        <repo-id>/
            base/
    workspaces/
        <workspace-id>/
            repo/
    state/
```

`~/.otter/repos/suger-api/base` acts as the canonical backing checkout.
Workspace worktrees (`~/.otter/workspaces/ws_01/repo`, `ws_02/repo`, …) share
Git objects:

```
              shared repository
                     │
              Git object store
                     │
          ┌──────────┼──────────┐
          ▼          ▼          ▼
        ws_01      ws_02      ws_03
       worktree   worktree   worktree
```

## 14. Git Workspace Creation

For a repository not previously used on a Host:

```
Create Workspace
      │
      ▼
Clone repository into ~/.otter/repos/<repo-id>/base
      │
      ▼
Fetch base revision
      │
      ▼
Create branch if required
      │
      ▼
git worktree add
      │
      ▼
~/.otter/workspaces/<workspace-id>/repo
```

For an existing repository:

```
fetch → git worktree add → workspace ready
```

Concurrency around Git fetch/worktree operations must be handled safely.

## 15. Existing Directory Workspaces

Git should not be required. Otter should eventually support at least these
Workspace sources:

1. Empty workspace
2. Git repository
3. Existing directory

V1 may initially implement only Empty + Git if necessary.

## 16. Development Environment

Otter must not become a package manager. The repository owns its development
environment.

For the primary development workflow:

```
Git repository
      │
      ▼
.envrc
      │
      ▼
direnv
      │
      ▼
flake.nix / flake.lock
      │
      ▼
Nix
```

Otter's responsibility is environment activation, not dependency management.

## 17. Environment Provider

The V1 conceptual model should support two environment kinds: **None** and
**Direnv**.

For Direnv:

```
detect()
prepare()
resolve_environment()
status()
```

The exact interface may evolve during implementation.

All managed Sessions in a Workspace must inherit the same resolved environment.

Do not depend on interactive shell startup hooks to activate direnv. Otter
should explicitly obtain/activate the environment before launching managed
processes.

## 18. Workspace Creation with Direnv

For a Git-backed repository:

```
create worktree
      │
      ▼
enter workspace root
      │
      ▼
authorize/prepare direnv
      │
      ▼
Nix realizes flake dependencies
      │
      ▼
environment ready
      │
      ├── start Codex
      ├── start shell
      ├── start server
      └── run tasks
```

Environment preparation may take time and therefore needs observable state:
`preparing`, `ready`, `failed`.

## 19. Credentials

Do not build centralized credential management in V1. Use credentials already
available on the remote host, for example: Git authentication, Codex
authentication, GitHub CLI authentication, AWS credentials, GCP credentials, npm
credentials.

The expectation for V1 is:

```
ssh dev-01
git clone <private-repository>    # already works
codex                             # already works
```

If those commands work manually, Otter should reuse the same environment.

SSH connectivity from the control plane should similarly use the user's
existing SSH configuration / SSH agent.

Future versions may introduce credential profiles and secure secret
distribution.

**Never persist arbitrary environment variables or secrets into the event log.**

## 20. Events

Events should be first-class.

Example events:

```
WorkspaceCreated      WorkspaceStarted      WorkspaceArchived
SessionCreated        SessionStarted        SessionStopped
ExecutionStarted      ExecutionExited       ExecutionFailed
EnvironmentPreparing  EnvironmentReady      EnvironmentFailed
AgentStarted          AgentWorking          AgentWaitingForInput
AgentCompleted        AgentFailed
TaskStarted           TaskCompleted         TaskFailed
ServiceStarted        ServiceStopped        ServiceFailed
AttentionCreated      AttentionResolved
```

Do not require full event sourcing for V1. However, events should be structured
rather than being derived solely from logs.

## 21. Attention

The key product abstraction is human attention. The system should answer:

> Which work requires the developer right now?

Initial attention categories: `question`, `approval`, `failure`, `review`,
`completion`.

```yaml
workspace: ws_01
session: codex
type: question
priority: blocked
summary: Codex needs a decision.
created_at: ...
```

V1 does not need sophisticated prioritization. At minimum, the UI should
distinguish: **Needs You**, **Working**, **Completed**, **Failed**.

## 22. Agent Integration

Codex is the first supported coding agent. However:

```
Workspace != Codex
Session   != Codex
```

Codex should be implemented as an Agent provider/integration:

```
Agent Provider
    Codex
```

Future implementations might include Claude Code or other CLI agents. Do not
build those integrations in V1. *(Superseded during implementation — Claude
Code was added as a second provider, `agents/claude.rs`; see
[decisions.md D-023](decisions.md).)*

## 23. Agent State Detection

This remains an implementation investigation. *(Current approach: each
provider reads its agent's own transcript, plus a shared quiet-turn heuristic
for prompts the transcript doesn't show; see
[decisions.md D-013 and D-023](decisions.md).)*

Desired states: `starting`, `working`, `waiting_for_input`, `completed`,
`failed`.

Prefer structured Codex events/APIs if available. Do not make
terminal-output-idleness heuristics the primary architecture unless no
structured mechanism exists. A temporary heuristic adapter is acceptable for
early dogfooding.

Current choice ([D-035](decisions.md)): Claude Code reports permission
prompts and questions through hooks passed per launch; Codex has no usable
structured signal, so a still screen *and* transcript mid-turn is read as
waiting (calibrated against the real TUIs).

## 24. Host Daemon Responsibilities

`otterd` on each Host owns:

```
WorkspaceManager
SessionManager
ExecutionManager
GitManager
EnvironmentManager
TmuxBackend
AgentManager
EventEmitter
```

The daemon should be able to operate independently while the Mac client is
disconnected. Closing the Mac application or putting the Mac to sleep must not
terminate remote work.

## 25. Control Plane Responsibilities

The control plane (the `otter` CLI first, then the Mac app) owns:

```
Host registry
SSH connectivity
Otter connections
Global workspace index
Attention aggregation
User-facing workspace lifecycle
UI state
```

The control plane should present Workspaces independently of where they run.
Host placement is metadata, not the primary navigation model.

## 26. UI Mental Model

Primary UI:

```
NEEDS YOU
🔴 GCP renewal
   Codex needs a decision
🔴 Stripe migration
   Tests failed

WORKING
🟢 IAM cleanup
   Codex working
🟢 Billing refactor
   Tests running

COMPLETED
✓ SES retry
✓ Pricing validation
```

Selecting a Workspace opens:

```
GCP Renewal
[Agent] [Shell] [Server] [Tests]
---------------------------------
         active session
---------------------------------
branch       environment      state
```

The UI should feel closer to browser tabs + an inbox than a traditional terminal
multiplexer.

## 27. Fast Workspace Creation

Creating a Workspace must remain extremely cheap.

```
⌘N
Name: scratch
[Create]
```

Result:

```
Workspace
├── Codex
└── Shell
```

Structured creation can optionally specify: name, host, repository, base
revision, brief, initial sessions.

Do not require project-management metadata before the user can start working.

## 28. Workspace Lifecycle

Target lifecycle:

```
CREATE
   │
   ▼
PREPARING
   │
   ├── filesystem
   ├── git worktree
   └── environment
   │
   ▼
READY
   │
   ▼
ACTIVE
   │
   ├── working
   ├── needs-attention
   └── failed
   │
   ▼
COMPLETED
   │
   ▼
ARCHIVED
   │
   ▼
CLEANED
```

Archiving and deleting should be distinct operations.

Archiving (`workspace.archive`, D-036) puts a ready or failed workspace away:
its running sessions are stopped (kept, not deleted), its attention is
resolved, and its files, Git worktree, branch and uncommitted changes stay on
the host. An archived workspace runs nothing, raises no attention and is
hidden from default views (`otter ls`, the app's main groups). Unarchiving
prepares it again (files checked, environment re-resolved) back to READY;
its sessions stay stopped until restarted, which creates new executions.
COMPLETED and CLEANED are not separate states (yet).

Deletion may destroy: tmux sessions, processes, the workspace directory, the Git
worktree. It should **not** automatically destroy the shared backing
repository.

## 29. Managed vs Attached Processes

V1 should distinguish processes Otter owns from things it merely discovers.

- **Managed** — Otter controls lifecycle.
- **Attached** — existing external resource.

Automatic cleanup must only affect managed resources. Initial implementation may
support only managed resources.

## 30. Persistence

Durable:

```
Workspace definitions
Briefs
Session definitions
Execution history
Events
Attention state
Git metadata
Agent resume identifiers
```

Ephemeral / reconstructable:

```
UI selection
SSH tunnel
client connection
live terminal stream
```

Restarting Otter should allow it to reconcile its persisted state with existing
tmux sessions.

## 31. Failure Model

At minimum handle:

- Mac app closes
- Mac sleeps
- SSH disconnects
- Otter restarts
- Session process exits
- tmux session disappears
- Git operation fails
- direnv/Nix preparation fails
- Codex exits unexpectedly

The remote Workspace should survive client connectivity loss. Failures should
become structured Events and, where appropriate, Attention items.

## 32. Implementation Stack & Repository Layout

`otterd` sits directly on the OS boundary (process lifecycle, PTYs, tmux,
signals, Unix sockets, SSH plumbing, Git/direnv subprocesses, streaming events,
long-running concurrent sessions), and is expected to drift toward systems
programming rather than backend-service programming. V1 is written in
**Rust**.

Both Rust and Go keep the operational model of a single binary
(`scp otterd dev-box:~/.local/bin/`); a Node runtime on every host would not.

The Mac control plane is **Tauri** ([decisions.md D-018](decisions.md)), whose core is also Rust, so
domain and protocol types (`Workspace`, `Session`, `Execution`, `HostStatus`,
`Attention`, `WorkspaceEvent`, `AgentState`, …) are literally the same serde
types on every side.

```
otterd/
├── crates/
│   ├── core/       ← Workspace / Session / Execution domain types
│   ├── protocol/   ← shared RPC + event types, wire framing
│   ├── client/     ← connection to otterd (SSH / local transport)
│   ├── daemon/     ← `otterd` binary (runs on each host)
│   └── cli/        ← `otter` binary (control plane CLI)
│
└── apps/
    └── desktop/    ← Tauri app (D-018)
```

`otterd` and `otter` live in one Cargo workspace but stay conceptually separate:

```
otter                     otterd
  │                           │
  │ RPC                       │
  └──────────────────────────►│
                              │
                         WorkspaceManager
                         SessionManager
                         GitManager
                         TmuxBackend
                         DirenvProvider
```

The entire backend can be built and tested without Tauri. The Tauri UI is
"just" another client of the same API.

## 33. Suggested V1 Implementation Order

**Phase 1 — Host + daemon:** `otterd` daemon, SSH tunnel, ping/status, host
capabilities.

**Phase 2 — Basic workspace:** create empty workspace, list workspaces, delete
workspace. Filesystem: `~/.otter/workspaces/<id>`.

**Phase 3 — tmux sessions:** create shell session, attach/read/write, resize,
stop/restart.

**Phase 4 — Codex:** launch Codex, interact with Codex, persist session
identity, reconnect. At this point the system should already be dogfoodable.

**Phase 5 — Git:** repo cache, clone/fetch, worktree create, worktree cleanup.

**Phase 6 — direnv/Nix:** detect direnv, prepare environment, launch all
sessions inside the resolved environment, report preparation status.

**Phase 7 — Events + attention:** structured session/agent lifecycle events and
global attention aggregation.

**Phase 8 — CLI-first dogfooding (explicit gate before any GUI).** Use
`otter` daily against real hosts until the Workspace / Session / Attention
abstractions feel excellent from the terminal alone, e.g.:

```
otter host add dev-01
otter workspace create --host dev-01 \
    --repo git@github.com:sugerio/foo.git --name billing
otter session start billing --agent codex
otter list

WORKSPACE       HOST       STATE        SESSION
billing         dev-01     NEEDS YOU    codex
gcp-renewal     dev-01     WORKING      codex
scratch         dev-02     IDLE         shell
```

This validates the abstractions much faster than building the visual
application at the same time. `otter` is built alongside every phase above;
this phase is the gate, not the start.

**Phase 9 — Desktop UI (Tauri):** workspace dashboard, attention inbox,
workspace tabs, session tabs, timeline — as another client of the same API.

## 34. Critical Architectural Rules

These rules should be preserved during implementation:

1. Git is optional.
2. Codex is an integration, not the core abstraction.
3. tmux is an execution backend, not the core abstraction.
4. SSH is the only V1 transport.
5. A Workspace belongs to exactly one Host in V1.
6. Multiple Sessions may exist inside a Workspace.
7. Session identity is separate from process/Execution identity.
8. Workspaces survive client disconnects.
9. The repository owns its dependency environment.
10. Otter does not replace Nix or direnv.
11. Credentials remain on existing machines for V1.
12. Shared Git repositories should back multiple worktrees.
13. Human attention is a first-class product concept.
14. Starting an unstructured scratch Workspace must remain extremely cheap.
15. Infrastructure details should not dominate the primary UX.

## 35. Open Decisions

These are intentionally not settled by this document. Current choices live in
[`decisions.md`](decisions.md) and may change.

- **Otter RPC protocol** — framing, method naming, streaming/attach model,
  versioning.
- **Persistence format** — on-disk representation of durable state and events.
- **Codex state detection** — structured Codex events/APIs vs. a temporary
  heuristic adapter (still the heuristic; D-013, D-035).

## 36. V1 Success Criterion

The first meaningful dogfooding milestone is:

> From the Mac application, create several Workspaces on a remote VM, launch
> Codex in each, switch between them instantly, disconnect/reconnect without
> losing anything, and clearly see which Codex session currently needs human
> input.

Then add:

> Create Git-backed Workspaces using shared repository storage, isolated
> worktrees, and the repository's direnv/Nix environment.

If those two workflows feel dramatically better than manually navigating SSH +
tmux sessions, the architecture is proving its value.

The objective of V1 is not to build a complete distributed development
platform. The objective is to make 5–10 concurrent coding workstreams feel as
manageable as 5–10 browser tabs.
