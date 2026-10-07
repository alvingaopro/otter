# workd — Architecture Lessons and Hardening Brief

> An architecture review/hardening brief, not a refactoring plan. Audit the
> code against it and change only what the evidence justifies. The concrete
> outcome of the first audit is recorded in
> [`decisions.md` D-015](decisions.md); the invariants are maintained in
> [`invariants.md`](invariants.md).

## Context

We reviewed the architecture of workd and compared it with Cindy
(makecindy/cindy), a mature coding-agent desktop application that supports
multiple agent harnesses, remote execution, persistent sessions, SSH, remote
filesystem access, and multi-agent orchestration.

The purpose of this document is not to make workd resemble Cindy. The
comparison mainly validates several architectural decisions already made in
workd and reveals a few boundaries worth strengthening before we build more UI
and higher-level features.

The core conclusion:

> workd's current architecture is fundamentally sound. Do not perform a broad
> redesign. Strengthen a few boundaries, then dogfood the system and let real
> usage drive further changes.

## 1. Preserve the core domain model

```
Host
  └── Workspace
        └── Session
              └── Execution
```

These concepts must remain distinct:

```
Workspace      != Git repository
Workspace      != worktree
Workspace      != directory
Workspace      != agent session
Session        != process
Session        != tmux session
Session        != Codex session
Execution      != SSH connection
Git repository != Workspace
Host           != SSH connection
```

Implementation mechanisms will change over time (today: Execution → tmux,
Transport → SSH, Agent → Codex) while the domain concepts should remain
stable.

## 2. Workspace must remain Git-optional

A workspace is a durable working environment located on a Host. Git is one
possible source/provisioning mechanism:

```
Workspace
├── Empty directory          workctl new scratch
├── Existing directory       workctl new experiment --dir ~/experiments/foo
└── Git-backed workspace     workctl new feature-x --repo git@github.com:org/repo.git
      └── managed worktree
```

Git concepts must not leak into the generic Workspace model (no `repo`,
`branch`, `worktree` fields on `Workspace`); Git-specific metadata belongs to a
Git-backed source.

## 3. Keep the shared repository / worktree optimization

```
~/.workd/
├── repos/<repo-id>/base
└── workspaces/{ws-001, ws-002, ws-003}/
```

Shared object database, independent working trees: less disk, faster
creation, independent branches, isolated agent modifications, natural
parallelism later. This optimization belongs to the Git provisioning layer, not
the generic Workspace abstraction.

## 4. Keep one generic host daemon

```
Control Plane ──SSH──► workd
                         ├── WorkspaceManager
                         ├── SessionManager
                         ├── ExecutionManager
                         ├── GitManager
                         ├── EnvironmentManager
                         ├── AgentManager
                         ├── File operations
                         ├── Port forwarding support
                         └── EventEmitter
```

The daemon represents the capabilities of a machine. Do not create
`codex-daemon`, `claude-daemon`, `file-daemon`, `git-daemon` without a strong
operational reason. One host daemon, multiple internal capabilities.

## 5. SSH is transport, not the domain model

```
Desktop ──SSH──► workd dial ──► ~/.workd/run/workd.sock ──► workd daemon
```

SSH handles authentication and encryption; no daemon TCP port; Unix socket
permissions protect local access; existing SSH configuration is reused; the
daemon survives client disconnects. A Host is durable identity/configuration;
SSH is how the control plane reaches it.

## 6. Sessions must be durable

The UI must not own the lifetime of an agent process.

```
Desktop ─► workd ─► Session ─► Execution ─► Codex
```

If the desktop disconnects, the execution keeps running; later the desktop
reconnects and attaches to the session. Detach/reattach is a core property of
the system, not a convenience.

## 7. Strengthen the agent provider boundary

This is the primary hardening opportunity. Codex-specific behavior lives
behind an explicit provider boundary:

```
AgentManager ─► AgentProvider ─► CodexProvider
                              (future: ClaudeCode, OpenCode, Gemini — not now)
```

Adding a provider later must not require redesigning Workspace or Session. An
illustrative (not required) interface: `id()`, `detect(host)`, `launch(ctx)`,
`resume(ctx)`, `observe(execution)`.

## 8. Codex-specific knowledge belongs inside CodexProvider

Rollout files, resume identifiers, CLI arguments, `--no-daemon`, transcript
parsing, state detection, quiet-state heuristics, attention inference: inside
the provider. Generic code asks "what is the state of this agent session?",
not "where is the Codex rollout file?".

## 9. Provider state should be opaque

Not `codex_rollout_path` / `codex_resume_id` on `Session`, but:

```
Session
├── id, workspace_id
├── kind = agent
├── provider = codex
└── provider_session_id = opaque value
```

The provider owns the meaning of its identifiers.

## 10. Sessions and agents are different concepts

A workspace holds sessions — terminals, services, tasks, one or more agents —
with an agent being one kind of session. `Workspace → Agent` is too
restrictive.

## 11. Agent capability discovery

Ask "what agent providers can this Host run?", not "does this machine have
Codex?":

```
HostCapabilities
├── execution:   tmux
├── environment: nix, direnv
├── source:      git
└── agents
    ├── codex:  installed, version, launch, resume
    └── claude: installed: false
```

Don't over-design: provider identity, availability, version, what the control
plane needs.

## 12. Avoid local/remote branches throughout the codebase

No `if remote { … } else { std::fs::read(…) }` scattered through the
application. A remote workspace's path refers to the remote machine; the
control plane must never call local filesystem APIs for it.

## 13. Keep execution backend details out of Session

`Session → Execution → ExecutionBackend → tmux`. Don't expose tmux
identifiers through generic domain models except for diagnostics/internal
bookkeeping.

## 14. Environment setup is a workspace capability

```
Workspace provision
├── source preparation
├── environment preparation (direnv, nix)
└── ready
```

Agents inherit the workspace environment; Codex is not responsible for
understanding Nix/direnv.

## 15. Credentials are host/control-plane concerns

No secret-management system yet; existing host configuration (`~/.ssh`,
`~/.gitconfig`, agent logins) is fine for V1. But don't design APIs that
assume credentials always come from the user's shell — a later credential
store should not require redesigning Workspace or Session.

## 16. Multi-agent is a future layer, not a workd responsibility today

workd provides reliable primitives — create workspace, create session, launch
/ observe / attach / detach / stop / resume / inspect / destroy — and
higher-level software can orchestrate them later. Don't build an orchestrator
now.

## 17. Do not build a plugin framework yet

An internal Rust trait is sufficient. No dynamic loading, manifests,
marketplace, WASM runtime, or generic extension API without a real
requirement.

## 18. Keep the protocol boring

JSONL RPC + binary terminal frames + event stream. No gRPC / WebSocket / REST
/ GraphQL without a concrete requirement. Optimize for debuggability,
stability, simple versioning, easy CLI use, easy SSH transport.

## 19. Treat attention as a first-class concept

The control plane needs "which sessions need me?", not "which processes are
running?". Keep the state set small (running, idle, waiting_for_user,
completed, failed, unknown — or similar). Providers infer it differently:

```
provider-specific observation ─► generic session/attention state ─► UI
```

## 20. Design the UI around domain concepts

Hosts → Workspaces → Sessions; not SSH connections → tmux → Codex processes.
Implementation details belong in diagnostics. The primary questions: where is
my work, what is running, what needs my attention?

## 21. Use architecture documentation as an agent interface

`AGENTS.md` should route: modifying workspace lifecycle → read the workspace
model; RPC → protocol; Codex → agent providers; architecture → design and
decisions. This keeps coding agents from violating system-level invariants by
reading only the code around a task.

## 22. Architectural invariants

1. A Workspace does not require Git.
2. A Workspace may contain multiple Sessions.
3. A Session survives control-plane disconnection when appropriate.
4. The control plane does not own agent process lifetime.
5. Agent-provider-specific details do not leak into generic Workspace logic.
6. Codex-specific details do not define the generic Session model.
7. tmux is an execution backend, not the Session abstraction.
8. SSH is a transport, not the Host abstraction.
9. A remote Workspace's filesystem lives on its Host, not on the control-plane
   machine.
10. Git worktrees are an optimization/implementation of Git-backed workspace
    provisioning, not the Workspace abstraction itself.
11. The daemon is authoritative for runtime state.
12. The desktop/UI should be disposable and reconnectable.

## 23. Immediate hardening checklist

- **Agent abstraction:** launch, resume, rollout discovery, and state
  inference are behind the agent layer and provider-specific; generic Session
  code does not parse Codex artifacts; provider identifiers are opaque outside
  the provider.
- **Host capabilities:** the host reports available agent providers, with
  Codex availability/version, without hard-coding Codex as the only agent.
- **Workspace:** Git optional; existing and empty directories valid;
  Git-specific fields don't leak into generic state.
- **Session:** multiple per workspace; agent is a session kind; tmux behind
  the execution layer; detach/reconcile/re-attach work.
- **Remote:** filesystem operations run on the Host; no local fs assumptions
  in control-plane logic; SSH disconnect doesn't destroy durable sessions.

## 24. What not to build during this pass

Claude Code / OpenCode / Gemini integrations, multi-agent orchestration, a
generic scheduler, a cloud control plane, a mobile client, an MCP framework, a
memory or skill system, a plugin marketplace, a generic secret manager,
additional remote transports, container orchestration, Kubernetes.

## 25. Recommended development sequence

```
current implementation
  → small architecture hardening pass
  → CLI dogfooding
  → real-world friction discovered
  → fix abstractions where evidence justifies it
  → Tauri UI
```

Not: more speculative architecture → plugin system → multi-agent → several
providers → eventually dogfood.

## 26. Dogfooding questions

- **Workspace:** is creating/switching/deleting natural? Do scratch/non-Git
  workspaces feel first-class?
- **Sessions:** do users think in sessions? Is the workspace/session
  distinction useful? Can one workspace comfortably hold an agent, a terminal,
  a dev server and tests at once?
- **Attention:** can the system reliably answer "what needs my attention right
  now?" Does the state model need adjustment?
- **Persistence:** can the laptop UI disappear and reconnect without worry?
- **Remote:** does local vs remote feel almost identical from the control
  plane?
- **Agent:** where do Codex assumptions leak into generic concepts? If
  replacing Codex with another CLI agent would require changing Workspace or
  generic Session logic, investigate that boundary.

## 27. Longer-term architecture

If the primitives prove out, a control plane can schedule work across hosts
and workspaces (Task → Workspace A/Codex, Workspace B/Claude, Workspace
C/tests). That orchestration is built on top of workd, not embedded in its
fundamental abstractions.

## 28. Final principle

> workd manages durable development environments and processes. Coding agents
> are powerful workloads running inside those environments.

Not "workd is a Codex manager", and not yet "workd is a multi-agent
orchestration platform".

```
Host → Workspace → Session → Execution
                      ├── Agent ─► AgentProvider ─► Codex
                      ├── Shell
                      └── Service
```

Keep the core small, durable, observable, and reconnectable. Make the
generic-agent/provider boundary clean, then stop expanding the architecture
and dogfood the system.
