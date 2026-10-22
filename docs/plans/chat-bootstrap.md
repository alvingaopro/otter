# Chat bootstrap: say what to do, Otter finds or makes the workspace

**Status:** P1 done (D-179); P2 (cross-host choice in the CLI and app) next. **Branch:** `chat-bootstrap`.

## 1. The problem

A task needs a workspace, and a workspace needs a host and (usually) a
repository. Today the developer chooses all three by hand before the first
word of the task. Otter's host conversation (D-093) can make a task
(`create_task`), but only in a workspace that already exists.

## 2. What the developer gets

Open a chat, say what to do. Otter finds the workspace, or makes one from a
repository the developer named, and starts the task. When it can't tell, it
asks in the conversation.

## 3. What exists already (checked in the code)

- `workspace_create` takes Git, directory and empty sources and prepares Git
  ones in the background (`workspaces.rs`).
- A task waits while its workspace is being prepared (D-080).
- A task with no workspace is not an error: planning moves it to `Blocked`,
  "Choose a workspace for this task" (`controller.rs`, `plan_step`). No new
  state is added; the reason text points back to the conversation.
- `own_workspace` makes a worktree of the task's repository on a branch of its
  own, marked with `task_id` so D-105 archives it when the task ends. The
  backing repository stays (D-108), so the next task on it is quick.
- The host conversation is one per host (D-093). It stays one per host.

## 4. Design

### 4.1 `create_task` can make the workspace

`TaskCreate.workspace` becomes one of:

| Form | Meaning |
|---|---|
| `"name-or-id"` | an existing workspace (as now) |
| `{"new": {"repository", "base"?, "branch"?}}` | a workspace of its own for this task, from that repository |
| `"scratch"` | an empty managed directory |

`new` is `own_workspace`'s code with the source given instead of read from a
workspace: one function makes the name, the branch and the task's mark, so the
two cannot drift. The task and its workspace are made by one command, and a
repeat of the command makes neither twice (the applied check comes before
any workspace is made).

The old string form still parses (untagged enum), so older clients keep
working.

### 4.2 Finding

- `list_workspaces` gains each workspace's source repository, whether a task is
  working in it, and its brief, and takes an optional `query` (case-insensitive
  match on name, repository and brief). One tool, not a new name.
- `list_repositories` is new and read-only: the repositories already on this
  host (the backing repositories and the sources of its workspaces). It makes
  no network call.

### 4.3 Choosing

In order, the first that applies:

1. The developer named a workspace, a repository or a path: use it.
2. Exactly one workspace comes from the repository the request names: use it;
   if a task is working in it, make a workspace of its own from the same
   repository (`new`).
3. Not clear: Otter asks in plain text; the developer's answer is a new
   message, which is also the developer's word that `create_task` needs
   (`gateway.rs`, `DeveloperWord`).
4. Otter never makes up a repository: one comes from the developer's words,
   from `list_repositories`, or from its memory (D-113).

**Cloning is automatic** when the developer named the repository. A
repository Otter only inferred is confirmed first (rule 3).

### 4.4 Bootstrap

Otter chooses the source. Preparing it (the copy, the branch, the
environment) is the existing pipeline. Installing dependencies and running
setup are the first step of the task, done by the coding agent under policy;
Otter runs no command.

### 4.5 Across hosts

otterd knows one host. So choosing a host is done by the client, before the
host conversation, and the conversation stays one per host:

- The app's **New chat** and `otter chat` with no host call `workspace.list`
  on every connected host (existing request, cheap), and match the message
  text against workspace names and repositories (exact, no model).
- One host matches: the chat opens there. Several or none: the client asks
  which host, offering the last one used first.
- The host a chat went to is shown in its header; the developer can switch
  before sending.

A host-scope question card (a decision with no task) would be a new protocol
surface; it is not in P1. In P1 Otter asks in text.

## 5. Safety

- `create_task` stays `DeveloperWord`: only a developer message can start it.
- The repository is cloned with Otter's own git account (D-133); a failure is
  said in the task, never retried with another identity.
- Nothing is put on a command line (invariant 13); only managed directories
  are cleaned (invariant 10).
- User-visible wording says "preparing the workspace", not the tool used
  (AGENTS.md).

## 6. Phases

1. **P1, otterd:** `TaskCreate.workspace` forms; idempotent order in
   `task_create`; `list_workspaces` fields and `query`; `list_repositories`;
   the Blocked reason; protocol doc; tests (extend the workspace-prep e2e).
2. **P2, clients:** the cross-host choice in the CLI (`otter chat`, a task
   with no workspace) and the app's New chat.
3. **P3:** a host-level question card; setup remembered per workspace.

## 7. Not decided here

- Whether the host-level question card should reuse `DecisionRequest`
  or be its own type (P3).
