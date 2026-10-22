//! Otter's tools (D-089): what a turn may look at and change, and the
//! checks — here, in otterd, never in the model.
//!
//! - **Observe** tools read the task, its history, its run, its
//!   decisions and evidence, and the conversation's older messages;
//!   everything they return is redacted (D-075) and bounded.
//! - **Routine**: pausing — the safe direction — in any turn.
//! - **On the developer's word**: forwarding their messages, redirecting,
//!   starting, resuming, retrying, approving the plan, accepting, a changed
//!   goal. Only in a turn the developer's message started; each change is
//!   applied once under `<that message>:<tool>` (forwarding keeps each
//!   message's own id, D-059). Noting something in Otter's memory (D-113)
//!   too: what Otter keeps comes from talking with the developer, never
//!   from an event.
//!
//! Every change goes through what `converse` does (`effective_intent`,
//! `intent_step`, `after_intent`): a tool and a classified message have the
//! same effect. A change is written to the task's history before it is
//! made (`ToolCalled`), and what came of it with it (`ToolDone`). A tool,
//! once called, finishes even when the turn stops waiting for it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use otter_core::TaskId;
use otter_core::redact::redact;
use otter_core::task::{Message, Task, TaskEvent, TaskStatus};
use otter_core::tools::ToolLevel;
use otter_protocol::{ErrorCode, RpcError};
use serde_json::{Value, json};

use crate::agents::excerpt;
use crate::brain::Intent;
use crate::controller::{effective_intent, intent_step, plan_question};
use crate::daemon::Daemon;
use crate::memory::{MAX_MEMORY, Memory};
use crate::tasks::Change;

/// What started Otter's turn (D-089 §3). Set by otterd, never by the model.
#[derive(Clone, Debug)]
pub enum Origin {
    /// The developer's unanswered messages, oldest first; the last one is
    /// the authority for what Otter does on their word.
    Developer { messages: Vec<Message> },
    /// A lifecycle event: no developer authority.
    Event { event_id: String },
}

impl Origin {
    pub fn name(&self) -> &'static str {
        match self {
            Origin::Developer { .. } => "developer",
            Origin::Event { .. } => "event",
        }
    }

    /// The developer's message the turn acts on, if a message started it.
    pub fn authority(&self) -> Option<&Message> {
        match self {
            Origin::Developer { messages } => messages.last(),
            Origin::Event { .. } => None,
        }
    }

    fn messages(&self) -> &[Message] {
        match self {
            Origin::Developer { messages } => messages,
            Origin::Event { .. } => &[],
        }
    }

    /// What a change made in this turn is applied under.
    fn command(&self, tool: &str) -> String {
        match self {
            Origin::Developer { messages } => match messages.last() {
                Some(m) => format!("{}:{tool}", m.id),
                None => format!("turn:{tool}"),
            },
            Origin::Event { event_id } => format!("{event_id}:{tool}"),
        }
    }
}

/// Which turns may call a tool (D-089 §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Observe,
    Routine,
    DeveloperWord,
}

/// A tool as the model sees it.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub class: Class,
    pub description: &'static str,
    /// Its arguments, as a JSON schema.
    pub input: Value,
}

fn none() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

fn limit(default: u64, max: u64) -> Value {
    json!({
        "type": "object",
        "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": max, "default": default}},
        "additionalProperties": false
    })
}

fn message_ids() -> Value {
    json!({
        "type": "object",
        "properties": {"message_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1}},
        "required": ["message_ids"],
        "additionalProperties": false
    })
}

/// What every conversation has (D-113): its older messages, and Otter's
/// memory.
fn conversation_tools() -> Vec<ToolSpec> {
    use Class::*;
    let memory = || json!({"type": "string", "enum": ["conversation", "host"]});
    vec![
        ToolSpec {
            name: "get_messages",
            class: Observe,
            description: "This conversation's messages, newest first and whole (the context has only the latest, cut short): before a message id to go further back; query to find the ones that mention it. Their ids aren't this turn's: tools that take message ids don't take them.",
            input: json!({
                "type": "object",
                "properties": {
                    "before": {"type": "string"},
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 30, "default": 10}
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "remember",
            class: DeveloperWord,
            description: "Note in your memory, a line, something the developer decided or settled that should outlast the recent messages: a decision, a preference, a constraint, what was agreed. memory: conversation (this conversation's) or host (every conversation on this host). Not progress, never secrets.",
            input: json!({
                "type": "object",
                "properties": {"memory": memory(), "note": {"type": "string"}},
                "required": ["memory", "note"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "edit_memory",
            class: DeveloperWord,
            description: "Correct your memory: old, copied exactly from it, becomes new (empty: dropped). For a note that changed or no longer holds, or to fold notes together when it is full.",
            input: json!({
                "type": "object",
                "properties": {"memory": memory(), "old": {"type": "string"}, "new": {"type": "string"}},
                "required": ["memory", "old", "new"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Otter's tools for a task's conversation (M1).
pub fn tools() -> Vec<ToolSpec> {
    use Class::*;
    let t = |name, class, description, input| ToolSpec {
        name,
        class,
        description,
        input,
    };
    vec![
        t(
            "get_task",
            Observe,
            "Where the task stands: status and why, its steps, criteria, gates, attempts, the report.",
            none(),
        ),
        t(
            "get_task_history",
            Observe,
            "The task's history, newest first, a line each.",
            limit(20, 100),
        ),
        t(
            "get_run_events",
            Observe,
            "The coding agent's latest run: its state, what it did lately, and what it last said.",
            limit(30, 100),
        ),
        t(
            "get_pending_decisions",
            Observe,
            "What waits on a decision, and whether only the developer may make it.",
            none(),
        ),
        t(
            "get_task_evidence",
            Observe,
            "Evidence gathered so far: checks run, their results.",
            limit(10, 50),
        ),
        t(
            "pause_task",
            Routine,
            "Pause the task: the coding agent stops; it can be resumed.",
            none(),
        ),
        t(
            "forward_to_agent",
            DeveloperWord,
            "Pass the developer's messages (by id), as they wrote them, to the coding agent: after its current turn.",
            message_ids(),
        ),
        t(
            "ask_agent",
            DeveloperWord,
            "Ask the coding agent the developer's question about the code (their messages, by id): a turn of its own after its current one, in which it can only read; its answer comes as its own message.",
            message_ids(),
        ),
        t(
            "redirect_agent",
            DeveloperWord,
            "Stop what the coding agent is doing now and give it the developer's messages (by id) instead.",
            message_ids(),
        ),
        t(
            "list_workspaces",
            Observe,
            "The workspaces on this host (where tasks do their work): id, name, state, the repository it comes from, its brief, and the task holding it now (held_by_task: another task there would wait; the developer's own agent sessions aren't counted). query: only those whose name, repository or brief has these words.",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "additionalProperties": false
            }),
        ),
        t(
            "list_repositories",
            Observe,
            "The repositories already on this host: the ones its workspaces come from. set_workspace can make the task a workspace of its own from one (new_workspace). query: only those with these words in them.",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "additionalProperties": false
            }),
        ),
        t(
            "set_workspace",
            DeveloperWord,
            "Say where this task works, when it has no workspace yet: exactly one of workspace (name or id of one that exists), new_workspace (a repository the developer named or list_repositories gave: a workspace of its own is made from it, and is prepared before the work goes on), or scratch (an empty directory). A task that is blocked for want of a workspace goes on by itself; a draft still needs start_task.",
            json!({
                "type": "object",
                "properties": {
                    "workspace": {"type": "string"},
                    "new_workspace": {
                        "type": "object",
                        "properties": {
                            "repository": {"type": "string"},
                            "base": {"type": "string"},
                            "branch": {"type": "string"}
                        },
                        "required": ["repository"],
                        "additionalProperties": false
                    },
                    "scratch": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
        ),
        t("start_task", DeveloperWord, "Start a draft task.", none()),
        t(
            "resume_task",
            DeveloperWord,
            "Resume a paused task.",
            none(),
        ),
        t(
            "retry_task",
            DeveloperWord,
            "Retry a failed task with a fresh budget.",
            none(),
        ),
        t(
            "approve_plan",
            DeveloperWord,
            "Approve the plan the developer was asked about.",
            none(),
        ),
        t(
            "revise_task",
            DeveloperWord,
            "The goal changed: plan again from where the work stands (the developer's messages say how).",
            none(),
        ),
        t(
            "set_answer_only",
            DeveloperWord,
            "Only an answer is wanted (on), or changes are fine again (off): the developer said so. On, the coding agent only reads, and a plan to change things comes to the developer first.",
            json!({
                "type": "object",
                "properties": {"on": {"type": "boolean"}},
                "required": ["on"],
                "additionalProperties": false
            }),
        ),
        t(
            "accept_task",
            DeveloperWord,
            "Accept the task in review; its gates still apply.",
            none(),
        ),
        t(
            "watch",
            DeveloperWord,
            "Keep an eye on something for the developer, as they asked (D-183): kind ci — the task's pull request's CI until every check has finished, it can't run, or the pull request is merged or closed (for_minutes: how long at most, default 120); kind time — look at this task again in in_minutes. what: what they asked for, in their words. It reads CI once as it sets the watch: now is what it read (say that, and when), or not_read says why it couldn't; done \"nothing to wait for\" means it already came to something (came_to), and nothing is watched — tell them what. When it comes to something later, a turn of its own tells them in this conversation. At most 3 at once.",
            json!({
                "type": "object",
                "properties": {
                    "what": {"type": "string"},
                    "kind": {"type": "string", "enum": ["ci", "time"]},
                    "in_minutes": {"type": "integer", "minimum": 1, "maximum": otter_core::watch::MAX_MINUTES},
                    "for_minutes": {"type": "integer", "minimum": 1, "maximum": otter_core::watch::MAX_MINUTES}
                },
                "required": ["what", "kind"],
                "additionalProperties": false
            }),
        ),
        t(
            "cancel_watch",
            DeveloperWord,
            "Stop a watch the developer wants stopped (its id is in get_task's watches): it won't come back.",
            json!({
                "type": "object",
                "properties": {"watch_id": {"type": "string"}},
                "required": ["watch_id"],
                "additionalProperties": false
            }),
        ),
    ]
    .into_iter()
    .chain(conversation_tools())
    .collect()
}

fn task_id(more: Value) -> Value {
    let mut props = json!({"task_id": {"type": "string"}});
    if let Value::Object(m) = more {
        props.as_object_mut().unwrap().extend(m);
    }
    json!({
        "type": "object",
        "properties": props,
        "required": ["task_id"],
        "additionalProperties": false
    })
}

/// Otter's tools in its conversation on the host (D-093): looking across
/// the host's tasks, and making, starting, resuming and pausing them. What
/// belongs to one task — guidance, its questions, its decisions — is said in
/// that task's own conversation.
pub fn host_tools() -> Vec<ToolSpec> {
    use Class::*;
    let t = |name, class, description, input| ToolSpec {
        name,
        class,
        description,
        input,
    };
    vec![
        t(
            "list_tasks",
            Observe,
            "The tasks on this host, newest first: id, title, status and why, what each waits on.",
            limit(30, 100),
        ),
        t(
            "list_workspaces",
            Observe,
            "The workspaces on this host (where tasks do their work): id, name, state, the repository it comes from, its brief, and the task holding it now (held_by_task: a new task there would wait; the developer's own agent sessions aren't counted). query: only those whose name, repository or brief has these words.",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "additionalProperties": false
            }),
        ),
        t(
            "list_repositories",
            Observe,
            "The repositories already on this host: the ones its workspaces come from. A task can start a workspace of its own from one (create_task new_workspace). query: only those with these words in them.",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "additionalProperties": false
            }),
        ),
        t(
            "get_task",
            Observe,
            "Where one task stands: status and why, its steps, criteria, gates, attempts, the report.",
            task_id(json!({})),
        ),
        t(
            "get_pending_decisions",
            Observe,
            "What waits on a decision in one task, and whether only the developer may make it.",
            task_id(json!({})),
        ),
        t(
            "get_run_events",
            Observe,
            "One task's coding agent: its latest run, what it did lately, what it last said.",
            task_id(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 100}})),
        ),
        t(
            "create_task",
            DeveloperWord,
            "Make a task of what the developer asked for: their message(s) by id are its request, word for word; the title is yours; where it works is at most one of: workspace (name or id of one that exists), new_workspace (a repository the developer named or list_repositories gave: a workspace of its own is made from it, and is prepared before the work starts), or scratch (an empty directory); answer_only if they only want an answer; start if they asked for it to start. If it isn't clear where, ask the developer first.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "message_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    "workspace": {"type": "string"},
                    "new_workspace": {
                        "type": "object",
                        "properties": {
                            "repository": {"type": "string"},
                            "base": {"type": "string"},
                            "branch": {"type": "string"}
                        },
                        "required": ["repository"],
                        "additionalProperties": false
                    },
                    "scratch": {"type": "boolean"},
                    "answer_only": {"type": "boolean"},
                    "start": {"type": "boolean"}
                },
                "required": ["title", "message_ids"],
                "additionalProperties": false
            }),
        ),
        t(
            "start_task",
            DeveloperWord,
            "Start a draft task.",
            task_id(json!({})),
        ),
        t(
            "resume_task",
            DeveloperWord,
            "Resume a paused task.",
            task_id(json!({})),
        ),
        t(
            "pause_task",
            Routine,
            "Pause a task: its coding agent stops; it can be resumed.",
            task_id(json!({})),
        ),
    ]
    .into_iter()
    .chain(conversation_tools())
    .collect()
}

/// The tools a turn in `scope` has.
pub fn tools_for(scope: &Scope) -> Vec<ToolSpec> {
    match scope {
        Scope::Task(_) => tools(),
        Scope::Host => host_tools(),
    }
}

/// Why a tool did nothing, and what would work instead.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    pub next: Option<String>,
}

/// The repository a workspace comes from, as it may be shown.
fn repository_of(w: &otter_core::Workspace) -> Option<String> {
    match &w.source {
        otter_core::WorkspaceSource::Git(g) => Some(g.repository.clone()),
        _ => None,
    }
}

/// Every word of `query` is in one of the `fields` (no words: all match).
fn matches_query(query: &[String], fields: &[&str]) -> bool {
    let hay = fields.join(" ").to_lowercase();
    query.iter().all(|w| hay.contains(w.as_str()))
}

fn fail(code: &'static str, message: impl Into<String>) -> ToolError {
    ToolError {
        code,
        message: message.into(),
        next: None,
    }
}

impl ToolError {
    fn next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }
}

impl From<RpcError> for ToolError {
    fn from(e: RpcError) -> Self {
        let code = match e.code {
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::InvalidArgument | ErrorCode::InvalidRequest => "invalid",
            _ => "retryable",
        };
        fail(code, e.message)
    }
}

/// A tool's answer, as the model gets it.
pub fn answer(result: &Result<Value, ToolError>) -> Value {
    match result {
        Ok(data) => json!({"ok": true, "data": data}),
        Err(e) => {
            let mut error = json!({"code": e.code, "message": redact(&e.message)});
            if let Some(next) = &e.next {
                error["next"] = json!(next);
            }
            json!({"ok": false, "error": error})
        }
    }
}

/// Where a turn is: a task's conversation, or Otter's on the host (D-093).
#[derive(Clone, Debug)]
pub enum Scope {
    Task(TaskId),
    Host,
}

/// One turn's tools: its origin, its scope, how many calls it has left.
#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

struct Inner {
    daemon: Arc<Daemon>,
    scope: Scope,
    origin: Origin,
    max_calls: usize,
    calls: AtomicUsize,
    /// What this turn changed, a line each.
    done: Mutex<Vec<String>>,
    /// The only tools this turn may call, when it is for one thing (D-186).
    only: Option<Vec<&'static str>>,
}

impl Gateway {
    pub fn new(daemon: Arc<Daemon>, scope: Scope, origin: Origin, max_calls: usize) -> Self {
        Gateway {
            inner: Arc::new(Inner {
                daemon,
                scope,
                origin,
                max_calls,
                calls: AtomicUsize::new(0),
                done: Mutex::new(Vec::new()),
                only: None,
            }),
        }
    }

    /// A turn that may call only the tools it is given: the same gateway,
    /// the same checks, for fewer.
    pub fn restricted(
        daemon: Arc<Daemon>,
        scope: Scope,
        origin: Origin,
        max_calls: usize,
        only: Vec<&'static str>,
    ) -> Self {
        Gateway {
            inner: Arc::new(Inner {
                daemon,
                scope,
                origin,
                max_calls,
                calls: AtomicUsize::new(0),
                done: Mutex::new(Vec::new()),
                only: Some(only),
            }),
        }
    }

    /// What this turn changed so far, a line each.
    pub fn done(&self) -> Vec<String> {
        self.inner.done.lock().unwrap().clone()
    }

    /// Call a tool. It runs to its end even if the turn stops waiting.
    pub async fn call(&self, name: &str, args: Value) -> Value {
        let g = self.clone();
        let name = name.to_owned();
        match tokio::spawn(async move { g.run(&name, args).await }).await {
            Ok(v) => v,
            Err(e) => answer(&Err(fail("retryable", format!("the tool stopped: {e}")))),
        }
    }

    async fn run(&self, name: &str, args: Value) -> Value {
        let result = self.checked(name, &args).await;
        if let Err(e) = &result {
            tracing::debug!(scope = ?self.inner.scope, tool = name, code = e.code, "Otter's tool");
        }
        answer(&result)
    }

    async fn checked(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
        let n = self.inner.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n > self.inner.max_calls {
            return Err(fail(
                "limit",
                format!("this turn has used its {} tool calls", self.inner.max_calls),
            )
            .next("answer with what you know"));
        }
        // The SDK's own tools, called as `@<name>` (D-191).
        if let Some(b) = name.strip_prefix('@') {
            return self.builtin(b, args).await;
        }
        let Some(spec) = tools_for(&self.inner.scope)
            .into_iter()
            .find(|t| t.name == name)
        else {
            return Err(fail("not_found", format!("there is no tool `{name}`")));
        };
        if let Some(only) = &self.inner.only
            && !only.contains(&name)
        {
            let e = fail(
                "not_allowed_in_this_turn",
                format!("`{name}` isn't for this turn, which only looks for where the task works"),
            )
            .next("answer with where the task will work, or ask the developer");
            self.record_done(name, Some(e.code)).await;
            return Err(e);
        }
        // How far it may go here (D-190): off isn't offered; `developer`
        // needs a turn the developer's message started; `auto` goes.
        let level = self.level(name).await;
        if level == ToolLevel::Off {
            let e = fail(
                "not_allowed_in_this_turn",
                format!("`{name}` is turned off for Otter here"),
            )
            .next("tell the developer; they can turn it on in Settings › Otter");
            self.record_done(name, Some(e.code)).await;
            return Err(e);
        }
        if !level.allows(self.inner.origin.authority().is_some()) {
            let e = fail(
                "not_allowed_in_this_turn",
                format!("`{name}` acts on the developer's word, and no message of theirs started this turn"),
            )
            .next("tell the developer what you would do; they can ask for it");
            self.record_done(name, Some(e.code)).await;
            return Err(e);
        }
        let _ = spec;
        if matches!(name, "remember" | "edit_memory") {
            return self.memory_tool(name, args).await;
        }
        let result = match (&self.inner.scope, spec.class) {
            (Scope::Host, Class::Observe) => self.host_observe(name, args).await,
            (Scope::Host, _) => self.host_write(name, args).await,
            (Scope::Task(id), Class::Observe) => self.observe(id, name, args).await,
            (Scope::Task(_), _) => self.write(name, args).await,
        };
        if spec.class == Class::Observe {
            self.record_done(name, result.as_ref().err().map(|e| e.code))
                .await;
        }
        result
    }

    /// One of the SDK's own tools (D-191), before it runs in Otter's turn:
    /// `{"allow": true}`, or `{"allow": false, "why": …}` for the model.
    /// Its level decides, as for any tool; `Bash` runs in Otter's empty
    /// directory and goes through the command policy too — routine runs,
    /// what only the developer may allow is refused (nobody can answer in
    /// the middle of a turn), what policy denies is denied. What the policy
    /// leaves to a model's judgement (D-174) is Otter's own: it is the model
    /// asking. Every call is on record, with what it was called on.
    async fn builtin(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
        if !crate::levels::BUILTINS.contains(&name) {
            return Err(fail("not_found", format!("there is no tool `{name}`")));
        }
        let refuse = |why: String| async move {
            self.record_done(name, Some("refused")).await;
            Ok(json!({"allow": false, "why": why}))
        };
        if let Some(only) = &self.inner.only
            && !only.contains(&name)
        {
            return refuse(format!("`{name}` isn't for this turn")).await;
        }
        let level = self.level(name).await;
        if level == ToolLevel::Off {
            return refuse(format!(
                "`{name}` is turned off for Otter here; the developer can turn it on in Settings › Otter"
            ))
            .await;
        }
        if !level.allows(self.inner.origin.authority().is_some()) {
            return refuse(format!(
                "`{name}` acts on the developer's word, and no message of theirs started this turn"
            ))
            .await;
        }
        let text = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default();
        let detail = match name {
            "Bash" => text("command").to_owned(),
            "WebSearch" => text("query").to_owned(),
            _ => text("url").to_owned(),
        };
        if name == "Bash" {
            use crate::runtime::policy::{self, Verdict};
            let dir = self.inner.daemon.otter_dir();
            let call = crate::runtime::ToolCall::Command {
                line: detail.clone(),
            };
            match policy::classify(&call, &dir) {
                Verdict::Allow
                | Verdict::Ask {
                    user_only: false, ..
                } => {}
                Verdict::Ask {
                    user_only: true,
                    why,
                    ..
                } => {
                    return refuse(format!(
                        "only the developer may allow this ({why}); tell them what you'd run"
                    ))
                    .await;
                }
                Verdict::Deny { why } => {
                    return refuse(format!("never allowed: {why}")).await;
                }
            }
        }
        let origin = &self.inner.origin;
        self.record(
            TaskEvent::ToolCalled {
                tool: name.into(),
                origin: origin.name().into(),
                authority: origin.authority().map(|m| m.id.to_string()),
                detail: Some(excerpt(&redact(&detail), 300)).filter(|d| !d.is_empty()),
            },
            None,
        )
        .await;
        self.record_done(name, None).await;
        self.inner
            .done
            .lock()
            .unwrap()
            .push(format!("{name}: {}", excerpt(&redact(&detail), 80)));
        Ok(json!({"allow": true}))
    }

    /// `name`'s level in this turn's scope: the task's (the host's,
    /// tightened by the task's own), or the host's.
    async fn level(&self, name: &str) -> ToolLevel {
        let d = &self.inner.daemon;
        match &self.inner.scope {
            Scope::Task(id) => match d.task_get(id.as_str()).await {
                Ok(f) => d.tool_level(Some(&f), name),
                Err(_) => d.tool_level(None, name),
            },
            Scope::Host => d.tool_level(None, name),
        }
    }

    /// The task this turn is about (a task's conversation).
    fn task(&self) -> &TaskId {
        match &self.inner.scope {
            Scope::Task(id) => id,
            Scope::Host => unreachable!("a host turn names its task"),
        }
    }

    async fn load(&self) -> Result<Task, ToolError> {
        Ok(self.inner.daemon.task_get(self.task().as_str()).await?)
    }

    /// Write `ToolDone` for a call that changed nothing (a read, a refusal).
    async fn record_done(&self, tool: &str, error: Option<&str>) {
        let event = TaskEvent::ToolDone {
            tool: tool.into(),
            error: error.map(Into::into),
        };
        self.record(event, None).await;
    }

    async fn record(&self, event: TaskEvent, because: Option<&str>) {
        let d = &self.inner.daemon;
        let change = |event: TaskEvent| {
            let c = Change::new(event);
            vec![match because {
                Some(id) => c.because(id),
                None => c,
            }]
        };
        let recorded = match &self.inner.scope {
            Scope::Task(id) => d
                .tasks
                .lock()
                .await
                .apply(id, None, |_, _| Ok(change(event)))
                .map(|a| d.task_changed(&a)),
            Scope::Host => d
                .chat
                .lock()
                .await
                .apply(None, |_, _| Ok(change(event)))
                .map(|a| d.chat_changed(&a)),
        };
        if let Err(e) = recorded {
            tracing::warn!(scope = ?self.inner.scope, "recording Otter's tool: {}", e.message);
        }
    }

    // --- Observe -----------------------------------------------------------

    async fn observe(&self, task: &TaskId, name: &str, args: &Value) -> Result<Value, ToolError> {
        let lim = |default: usize, max: usize| -> Result<usize, ToolError> {
            match args.get("limit") {
                None | Some(Value::Null) => Ok(default),
                Some(v) => v
                    .as_u64()
                    .filter(|n| (1..=max as u64).contains(n))
                    .map(|n| n as usize)
                    .ok_or_else(|| fail("invalid", format!("limit is a number from 1 to {max}"))),
            }
        };
        // A live run's activity isn't in the task until it ends.
        let f = self
            .inner
            .daemon
            .for_clients(self.inner.daemon.task_get(task.as_str()).await?);
        let r = |s: &str| redact(s);
        let short = |s: &str, n: usize| excerpt(&redact(s), n);
        match name {
            "list_workspaces" => self.workspaces_listing(args).await,
            "list_repositories" => self.repositories_listing(args).await,
            "get_task" => Ok(json!({
                "id": f.id.as_str(),
                "workspace": self.workspace_name(&f).await,
                "title": r(&f.title),
                "request": short(&f.request, 2000),
                "status": f.status.as_str(),
                "status_reason": f.status_reason.as_deref().map(r),
                // What only the developer can do now (D-134), if anything.
                "waits_on_you": f.needs().map(|n| r(&n.summary)),
                "steps": f.steps.iter().map(|t| json!({
                    "title": r(&t.title),
                    "status": format!("{:?}", t.status).to_lowercase(),
                })).collect::<Vec<_>>(),
                "criteria": f.acceptance.iter().map(|c| json!({"text": r(&c.text), "met": c.met})).collect::<Vec<_>>(),
                "check": f.verify_command.as_deref().map(r),
                "gates": f.gates.iter().map(|g| json!({
                    "name": g.name,
                    "status": format!("{:?}", g.status).to_lowercase(),
                    "detail": g.detail.as_deref().map(r),
                })).collect::<Vec<_>>(),
                "pending_decisions": f.pending_decisions().count(),
                "attempts": {"used": f.budget.iterations_used, "max": f.budget.max_iterations},
                "runs": f.runs.len(),
                "report": f.report.as_deref().map(|t| short(t, 4000)),
                // What CI and the pull request were, and when that was read
                // (D-184): never "just now" unless it was.
                "pull_request": f.delivery.as_ref().filter(|d| d.pr_url.is_some()).map(|d| json!({
                    "url": d.pr_url,
                    "conflicts_with_base": d.mergeable.as_deref() == Some(crate::delivery::CONFLICTING),
                    "ci": d.ci.iter().map(|c| json!({"name": r(&c.name), "state": c.state})).collect::<Vec<_>>(),
                    "read_at": self.inner.daemon.ci_read_at(&f.id).or(d.ci_read_at),
                    // A watch on it may have read it since: newer than the above.
                    "read_since_by_a_watch": f.watches.iter()
                        .filter(|w| w.kind == "ci")
                        .filter_map(|w| w.last_reading())
                        .filter(|x| Some(x.at) > self.inner.daemon.ci_read_at(&f.id).or(d.ci_read_at))
                        .max_by_key(|x| x.at)
                        .map(|x| json!({"at": x.at, "text": r(&x.text)})),
                })),
                "watches": f.watches.iter().rev().take(8).map(|w| json!({
                    "id": w.id.as_str(),
                    "kind": w.kind,
                    "label": r(&w.label),
                    "what": r(&w.what),
                    "status": format!("{:?}", w.status).to_lowercase(),
                    "until": w.deadline,
                    "last_read": w.last_reading().map(|x| json!({"at": x.at, "text": r(&x.text)})),
                    "came_to": w.finding.as_ref().map(|x| json!({"outcome": x.outcome.as_str(), "text": r(&x.text), "at": x.at})),
                })).collect::<Vec<_>>(),
            })),
            "get_task_history" => {
                let n = lim(20, 100)?;
                let all = self
                    .inner
                    .daemon
                    .tasks
                    .lock()
                    .await
                    .history(&f.id, 0, usize::MAX)
                    .map_err(|e| fail("retryable", format!("reading the history: {e:#}")))?;
                let lines: Vec<Value> = all
                    .iter()
                    .rev()
                    .take(n)
                    .map(|rec| json!({"seq": rec.seq, "at": rec.ts, "text": r(&rec.text)}))
                    .collect();
                Ok(json!({"newest_first": lines}))
            }
            "get_run_events" => {
                let n = lim(30, 100)?;
                let Some(run) = f.runs.last() else {
                    return Err(fail("not_ready", "the coding agent hasn't run yet"));
                };
                let activity = &run.activity;
                let skip = activity.len().saturating_sub(n);
                let last_said = run
                    .conversation_id
                    .as_ref()
                    .and_then(|c| self.inner.daemon.conversations.get(c.as_str()))
                    .and_then(|c| {
                        c.messages
                            .iter()
                            .rev()
                            .find(|m| !m.text().trim().is_empty())
                            .map(|m| short(&m.text(), 1500))
                    });
                let step = f.steps.iter().find(|t| t.id == run.step_id);
                Ok(json!({
                    "run": {
                        "id": run.id.as_str(),
                        "state": run.state.as_str(),
                        "step": step.map(|t| r(&t.title)),
                        "started_at": run.started_at,
                        "ended_at": run.ended_at,
                        "summary": run.summary.as_deref().map(|s| short(s, 1500)),
                        "worked_ms": run.worked(chrono::Utc::now()).num_milliseconds(),
                    },
                    "activity": activity[skip..].iter().map(|l| r(l)).collect::<Vec<_>>(),
                    "last_said": last_said,
                }))
            }
            "get_pending_decisions" => Ok(json!({
                "decisions": f.pending_decisions().map(|d| json!({
                    "id": d.id.as_str(),
                    "kind": format!("{:?}", d.kind).to_lowercase(),
                    "risk": format!("{:?}", d.risk).to_lowercase(),
                    "summary": r(&d.summary),
                    "detail": d.detail.as_deref().map(|t| short(t, 1500)),
                    "options": d.options.iter().map(|o| r(o)).collect::<Vec<_>>(),
                    "developer_only": d.user_only,
                })).collect::<Vec<_>>(),
            })),
            "get_messages" => messages_page(&f.messages, args),
            "get_task_evidence" => {
                let n = lim(10, 50)?;
                let skip = f.evidence.len().saturating_sub(n);
                Ok(json!({
                    "check": f.verify_command.as_deref().map(r),
                    "evidence": f.evidence[skip..].iter().map(|e| json!({
                        "kind": format!("{:?}", e.kind).to_lowercase(),
                        "title": r(&e.title),
                        "ok": e.ok,
                        "uncertain": e.uncertain,
                        "detail": e.detail.as_deref().map(|t| short(t, 1500)),
                        "at": e.at,
                    })).collect::<Vec<_>>(),
                }))
            }
            _ => Err(fail("not_found", format!("there is no tool `{name}`"))),
        }
    }

    // --- Memory (D-113) ----------------------------------------------------

    /// `remember` and `edit_memory`: this conversation's memory, or the
    /// host's. Not applied under a command id — one turn may note several
    /// things — but a note already there isn't added again, so a turn run
    /// again notes nothing twice.
    async fn memory_tool(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
        use otter_protocol::otter::MemoryScope;
        let origin = &self.inner.origin;
        self.record(
            TaskEvent::ToolCalled {
                tool: name.into(),
                origin: origin.name().into(),
                authority: origin.authority().map(|m| m.id.to_string()),
                detail: None,
            },
            None,
        )
        .await;
        let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
        let result = async {
            let (scope, which) = match text("memory") {
                "conversation" => (MemoryScope::Conversation, "this conversation's memory"),
                "host" => (MemoryScope::Host, "the host's memory"),
                _ => return Err(fail("invalid", "memory is conversation or host")),
            };
            let task = match &self.inner.scope {
                Scope::Task(id) => Some(id),
                Scope::Host => None,
            };
            let m = Memory::of(scope, task);
            let d = &self.inner.daemon;
            let mut did = "";
            let now = if name == "remember" {
                let note = redact(text("note").trim());
                if note.is_empty() {
                    return Err(fail("invalid", "note is what to remember"));
                }
                if note.chars().count() > MAX_NOTE {
                    return Err(fail(
                        "invalid",
                        format!("a note is at most {MAX_NOTE} characters"),
                    )
                    .next("keep to what was decided"));
                }
                d.change_memory(&m, |now| {
                    if noted(now, &note) {
                        did = "already noted";
                        return Ok(None);
                    }
                    let line = if note.starts_with(['-', '*', '#']) {
                        note.clone()
                    } else {
                        format!("- {note}")
                    };
                    let sep = if now.is_empty() || now.ends_with('\n') {
                        ""
                    } else {
                        "\n"
                    };
                    let next = format!("{now}{sep}{line}\n");
                    if next.trim().chars().count() > MAX_MEMORY {
                        return Err(fail(
                            "limit",
                            format!("the memory is full ({MAX_MEMORY} characters)"),
                        )
                        .next("edit_memory: fold notes together or drop what no longer holds, then remember this"));
                    }
                    did = "noted";
                    Ok(Some(next))
                })
                .await?
            } else {
                let old = text("old");
                let new = redact(text("new").trim());
                if old.trim().is_empty() {
                    return Err(fail("invalid", "old is text copied from the memory"));
                }
                d.change_memory(&m, |now| match now.matches(old).count() {
                    0 => Err(fail("not_found", "that text isn't in the memory")
                        .next("old is copied exactly from the memory you were given")),
                    1 => {
                        did = "edited";
                        Ok(Some(now.replacen(old, &new, 1)))
                    }
                    _ => Err(fail("invalid", "that text is in the memory more than once")
                        .next("give more of it, so it is found once")),
                })
                .await?
            };
            if did != "already noted" {
                self.inner
                    .done
                    .lock()
                    .unwrap()
                    .push(format!("{name}: {which}"));
            }
            Ok(json!({
                "done": did,
                "memory": text("memory"),
                "chars": now.chars().count(),
                "max_chars": MAX_MEMORY,
                "now": now,
            }))
        }
        .await;
        self.record_done(name, result.as_ref().err().map(|e| e.code))
            .await;
        result
    }

    // --- On the host (D-093) -----------------------------------------------

    /// The task a host tool names.
    fn named_task(args: &Value) -> Result<TaskId, ToolError> {
        args.get("task_id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|s| TaskId::from(s.trim()))
            .ok_or_else(|| fail("invalid", "task_id names the task (list_tasks has them)"))
    }

    /// The name of the workspace a task works in, if it has one.
    async fn workspace_name(&self, f: &otter_core::task::Task) -> Option<String> {
        let id = f.workspace_id.as_ref()?;
        let store = self.inner.daemon.store.lock().await;
        store.workspace(id.as_str()).map(|w| w.name.clone())
    }

    /// `list_workspaces`, in a task's conversation and the host's alike.
    async fn workspaces_listing(&self, args: &Value) -> Result<Value, ToolError> {
        let d = &self.inner.daemon;
        let query = Self::query(args);
        let all: Vec<otter_core::Workspace> = d.store.lock().await.state.workspaces.clone();
        let mut ws: Vec<Value> = Vec::new();
        for w in all {
            // Who has it now: a run there, or work of another task
            // there that is not done (D-079, D-156) — what would
            // make a new task wait.
            let held_by = match d.workspace_holder(&w.id) {
                Some(t) => Some(t),
                None => {
                    let git = std::path::Path::new(&w.root)
                        .ancestors()
                        .any(|p| p.join(".git").exists());
                    if git {
                        d.work_holding(&w.id, None).await.map(|t| t.id)
                    } else {
                        None
                    }
                }
            };
            let w = w.shown();
            let repository = repository_of(&w);
            if !matches_query(
                &query,
                &[
                    &w.name,
                    repository.as_deref().unwrap_or(""),
                    w.brief.title.as_deref().unwrap_or(""),
                    w.brief.description.as_deref().unwrap_or(""),
                ],
            ) {
                continue;
            }
            ws.push(json!({
                "id": w.id.as_str(),
                "name": w.name,
                "state": format!("{:?}", w.state).to_lowercase(),
                "repository": repository.as_deref().map(redact),
                "brief": redact(w.brief.title.as_deref().unwrap_or("")),
                "held_by_task": held_by.as_ref().map(|t| t.as_str()),
                "made_for_task": w.task_id.as_ref().map(|t| t.as_str()),
            }));
        }
        Ok(json!({"workspaces": ws}))
    }

    /// `list_repositories`, in a task's conversation and the host's alike.
    async fn repositories_listing(&self, args: &Value) -> Result<Value, ToolError> {
        let d = &self.inner.daemon;
        let query = Self::query(args);
        let store = d.store.lock().await;
        let mut repos: Vec<String> = store
            .state
            .workspaces
            .iter()
            .cloned()
            .map(otter_core::Workspace::shown)
            .filter_map(|w| repository_of(&w))
            .filter(|r| matches_query(&query, &[r]))
            .collect();
        repos.sort();
        repos.dedup();
        Ok(json!({"repositories": repos}))
    }

    async fn host_observe(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
        let d = &self.inner.daemon;
        match name {
            "list_tasks" => {
                let n = match args.get("limit") {
                    None | Some(Value::Null) => 30,
                    Some(v) => v
                        .as_u64()
                        .filter(|n| (1..=100).contains(n))
                        .ok_or_else(|| fail("invalid", "limit is a number from 1 to 100"))?
                        as usize,
                };
                let store = d.store.lock().await;
                let tasks: Vec<Value> = d
                    .tasks
                    .lock()
                    .await
                    .list()
                    .into_iter()
                    .take(n)
                    .map(|f| {
                        let workspace = f
                            .workspace_id
                            .as_ref()
                            .and_then(|w| store.workspace(w.as_str()))
                            .map(|w| w.name.clone());
                        json!({
                            "id": f.id.as_str(),
                            "title": redact(&f.title),
                            "status": f.status.as_str(),
                            "status_reason": f.status_reason.as_deref().map(redact),
                            "waits_on_developer": f.needs().map(|n| redact(&n.summary)),
                            "workspace": workspace,
                            "answer_only": f.answer_only,
                        })
                    })
                    .collect();
                Ok(json!({"newest_first": tasks}))
            }
            "get_messages" => {
                let chat = d.chat.lock().await;
                messages_page(&chat.get().messages, args)
            }
            "list_workspaces" => self.workspaces_listing(args).await,
            "list_repositories" => self.repositories_listing(args).await,
            _ => {
                let task = Self::named_task(args)?;
                let mut rest = args.clone();
                if let Some(m) = rest.as_object_mut() {
                    m.remove("task_id");
                }
                self.observe(&task, name, &rest).await
            }
        }
    }

    async fn host_write(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
        let result = self.host_change(name, args).await;
        if let Err(e) = &result {
            self.record_done(name, Some(e.code)).await;
        }
        result
    }

    async fn host_change(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
        use otter_core::task::TaskAction;
        use otter_protocol::task::{TaskAct, TaskCreate};
        let d = &self.inner.daemon;
        let origin = &self.inner.origin;
        let authority = origin.authority().map(|m| m.id.to_string());
        let called = |command: String| {
            let event = TaskEvent::ToolCalled {
                tool: name.into(),
                origin: origin.name().into(),
                authority: authority.clone(),
                detail: None,
            };
            (event, command)
        };
        if name == "create_task" {
            let title = args
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty() && t.len() <= 200)
                .ok_or_else(|| {
                    fail(
                        "invalid",
                        "title is the task's name, at most 200 characters",
                    )
                })?
                .to_owned();
            // The request is the developer's own words, never Otter's.
            let messages = self.chosen(args)?;
            let request = messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let workspace = Self::workspace_choice(args)?;
            let made_one = matches!(
                workspace,
                Some(
                    otter_protocol::task::WorkspaceChoice::New { .. }
                        | otter_protocol::task::WorkspaceChoice::Scratch { .. }
                )
            );
            let answer_only = args
                .get("answer_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let start = args.get("start").and_then(Value::as_bool).unwrap_or(false);
            let (event, command) = called(origin.command(name));
            self.record(event, Some(&command)).await;
            let f = d
                .task_create(TaskCreate {
                    command_id: command.clone(),
                    title,
                    request,
                    workspace,
                    answer_only,
                })
                .await?;
            let mut status = f.status;
            // A workspace that couldn't be made leaves a task without one:
            // not started, and said (D-179).
            let unplaced = made_one && f.workspace_id.is_none();
            if start && !unplaced {
                status = d
                    .clone()
                    .task_act(TaskAct {
                        command_id: format!("{command}:start"),
                        task: f.id.to_string(),
                        action: TaskAction::Start,
                    })
                    .await?
                    .status;
            }
            // Otter follows what it made (D-094), from where it stands now.
            let now = d.task_get(f.id.as_str()).await?;
            let applied = d.chat.lock().await.apply(None, |chat, _| {
                if !chat.follows.iter().any(|x| x.task_id == now.id) {
                    chat.follows.push(otter_core::task::Follow {
                        task_id: now.id.clone(),
                        seen: now.follow_key().unwrap_or_default(),
                    });
                }
                Ok(vec![])
            })?;
            d.chat_changed(&applied);
            self.record(
                TaskEvent::ToolDone {
                    tool: name.into(),
                    error: None,
                },
                Some(&command),
            )
            .await;
            self.inner
                .done
                .lock()
                .unwrap()
                .push(format!("create_task: {} ({})", f.title, f.id));
            if unplaced {
                return Ok(json!({
                    "done": "created",
                    "task_id": f.id.as_str(),
                    "title": f.title,
                    "status": status.as_str(),
                    "workspace": null,
                    "note": "the workspace could not be made, so the task was not started; the task says why. Tell the developer, and ask where it should work."
                }));
            }
            return Ok(
                json!({"done": "created", "task_id": f.id.as_str(), "title": f.title, "status": status.as_str()}),
            );
        }
        let task = Self::named_task(args)?;
        let action = match name {
            "start_task" => TaskAction::Start,
            "resume_task" => TaskAction::Resume,
            "pause_task" => TaskAction::Pause,
            _ => return Err(fail("not_found", format!("there is no tool `{name}`"))),
        };
        let (event, command) = called(format!("{}:{task}", origin.command(name)));
        self.record(event, Some(&command)).await;
        let f = d
            .clone()
            .task_act(TaskAct {
                command_id: command.clone(),
                task: task.to_string(),
                action,
            })
            .await?;
        self.record(
            TaskEvent::ToolDone {
                tool: name.into(),
                error: None,
            },
            Some(&command),
        )
        .await;
        self.inner
            .done
            .lock()
            .unwrap()
            .push(format!("{name}: {}", f.title));
        Ok(
            json!({"done": name.trim_end_matches("_task"), "task_id": f.id.as_str(), "status": f.status.as_str()}),
        )
    }

    // --- Changes -----------------------------------------------------------

    /// The words a listing is narrowed to, lowercased.
    fn query(args: &Value) -> Vec<String> {
        args.get("query")
            .and_then(Value::as_str)
            .map(|q| q.split_whitespace().map(str::to_lowercase).collect())
            .unwrap_or_default()
    }

    /// Where a new task works, from `create_task`'s arguments (D-179): at
    /// most one of a workspace, a repository to make one from, or scratch.
    fn workspace_choice(
        args: &Value,
    ) -> Result<Option<otter_protocol::task::WorkspaceChoice>, ToolError> {
        use otter_protocol::task::{NewWorkspace, Scratch, WorkspaceChoice};
        let named = args.get("workspace").and_then(Value::as_str);
        let new = args.get("new_workspace").filter(|v| !v.is_null());
        let scratch = args
            .get("scratch")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if [named.is_some(), new.is_some(), scratch]
            .iter()
            .filter(|b| **b)
            .count()
            > 1
        {
            return Err(fail(
                "invalid",
                "say where the task works one way: workspace, new_workspace or scratch",
            ));
        }
        Ok(if let Some(w) = named {
            Some(WorkspaceChoice::Named(w.to_owned()))
        } else if let Some(n) = new {
            let n: NewWorkspace = serde_json::from_value(n.clone())
                .map_err(|_| fail("invalid", "new_workspace is {repository, base?, branch?}"))?;
            Some(WorkspaceChoice::New { new: n })
        } else if scratch {
            Some(WorkspaceChoice::Scratch {
                scratch: Scratch {},
            })
        } else {
            None
        })
    }

    /// The developer's messages a forward or a redirect names: this turn's
    /// own, in their order.
    fn chosen(&self, args: &Value) -> Result<Vec<Message>, ToolError> {
        let ids: Vec<&str> = args
            .get("message_ids")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(fail(
                "invalid",
                "message_ids names the developer's messages to pass on",
            ));
        }
        let own = self.inner.origin.messages();
        if let Some(stray) = ids
            .iter()
            .find(|id| !own.iter().any(|m| m.id.as_str() == **id))
        {
            return Err(fail(
                "invalid",
                format!("{stray} isn't one of the messages this turn answers"),
            )
            .next(format!(
                "use these: {}",
                own.iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(own
            .iter()
            .filter(|m| ids.contains(&m.id.as_str()))
            .cloned()
            .collect())
    }

    /// A question for the coding agent at work (D-089 M2.5): a read-only
    /// turn of its own, on the SDK runtime only (where Otter's policy sees
    /// every tool call). Without a run at work, a question of its own is a
    /// task of its own (D-084).
    async fn ask(&self, f: &Task, args: &Value) -> Result<Value, ToolError> {
        let messages = self.chosen(args)?;
        let d = &self.inner.daemon;
        match d.live_backend(&f.id).as_deref() {
            None => {
                return Err(fail("not_ready", "the coding agent isn't at work on this task").next(
                    "a question on its own is answered as a task of its own: the developer can start one",
                ));
            }
            Some("sdk") => {}
            Some(_) => {
                return Err(fail(
                    "not_allowed_in_this_turn",
                    "code questions need the coding agent on the SDK runtime, where Otter can keep it to reading",
                )
                .next("in Settings, run the coding agent through the Claude Agent SDK worker"));
            }
        }
        let origin = &self.inner.origin;
        let command = origin.command("ask_agent");
        self.record(
            TaskEvent::ToolCalled {
                tool: "ask_agent".into(),
                origin: origin.name().into(),
                authority: origin.authority().map(|m| m.id.to_string()),
                detail: None,
            },
            Some(&command),
        )
        .await;
        let applied = d.tasks.lock().await.apply(&f.id, Some(&command), |_, _| {
            Ok(vec![
                Change::new(TaskEvent::ToolDone {
                    tool: "ask_agent".into(),
                    error: None,
                })
                .because(&command),
            ])
        })?;
        d.task_changed(&applied);
        // Each message once, by its own id (D-059): asked again (a turn run
        // again), it isn't queued twice.
        let carried = d.carried(f);
        let mut asked = 0;
        for m in messages.iter().filter(|m| !carried.contains(m.id.as_str())) {
            asked += d
                .ask_runs(&f.id, m.id.as_str(), &m.text, d.pictures_of(&f.id, m))
                .await;
        }
        if !applied.duplicate {
            self.inner
                .done
                .lock()
                .unwrap()
                .push("ask_agent: asked".into());
        }
        Ok(json!({"done": "asked", "turns": asked, "status": f.status.as_str()}))
    }

    /// Answer-only on or off, on the developer's word (D-092). On needs the
    /// SDK runtime, where the agent can be kept to reading.
    /// Say where a task works (D-186): one the developer named, one made
    /// from a repository they named, or an empty directory.
    async fn set_workspace(&self, f: &Task, args: &Value) -> Result<Value, ToolError> {
        let Some(choice) = Self::workspace_choice(args)? else {
            return Err(fail(
                "invalid",
                "say where: workspace, new_workspace or scratch",
            ));
        };
        let d = &self.inner.daemon;
        // D-080: not once it has worked, and not over a workspace it has
        // (the developer changes that in the app) — but the same choice
        // again, or the workspace made for it, is already done.
        if !f.runs.is_empty() {
            return Err(fail(
                "not_ready",
                "the coding agent has worked in this task's workspace already, and its work is there",
            ));
        }
        let placement = d.placement(&choice).await?;
        if let Some(cur) = &f.workspace_id {
            let (same, name, state) = {
                let store = d.store.lock().await;
                let ws = store.workspace(cur.as_str());
                let same = match (&placement, ws) {
                    (crate::tasks::Placement::Existing(id), _) => id == cur,
                    // The one made for this task, from what is asked now.
                    (crate::tasks::Placement::Make(source), Some(w)) => {
                        w.task_id.as_ref() == Some(&f.id)
                            && match (&w.source, source) {
                                (
                                    otter_core::WorkspaceSource::Git(g),
                                    otter_protocol::SourceSpec::Git { repository, .. },
                                ) => &g.repository == repository,
                                (
                                    otter_core::WorkspaceSource::Empty,
                                    otter_protocol::SourceSpec::Empty,
                                ) => true,
                                _ => false,
                            }
                    }
                    _ => false,
                };
                (
                    same,
                    ws.map(|w| w.name.clone()),
                    ws.map(|w| format!("{:?}", w.state).to_lowercase()),
                )
            };
            if !same {
                return Err(fail("conflict", "this task has a workspace already")
                    .next("the developer can give it another in the app"));
            }
            // Done already: nothing to do, nothing to record twice.
            return Ok(json!({
                "done": "workspace set",
                "workspace": name,
                "workspace_state": state,
                "status": f.status.as_str(),
            }));
        }
        let origin = &self.inner.origin;
        let command = origin.command("set_workspace");
        self.record(
            TaskEvent::ToolCalled {
                tool: "set_workspace".into(),
                origin: origin.name().into(),
                authority: origin.authority().map(|m| m.id.to_string()),
                detail: None,
            },
            Some(&command),
        )
        .await;
        let placed = d.place_task(f, placement, &command).await?;
        let (name, state) = {
            let store = d.store.lock().await;
            placed
                .workspace_id
                .as_ref()
                .and_then(|w| store.workspace(w.as_str()))
                .map(|w| (w.name.clone(), format!("{:?}", w.state).to_lowercase()))
                .unzip()
        };
        self.record(
            TaskEvent::ToolDone {
                tool: "set_workspace".into(),
                error: None,
            },
            Some(&command),
        )
        .await;
        self.inner.done.lock().unwrap().push(format!(
            "set_workspace: {}",
            name.as_deref().unwrap_or("none")
        ));
        Ok(json!({
            "done": "workspace set",
            "workspace": name,
            "workspace_state": state,
            "status": placed.status.as_str(),
        }))
    }

    async fn set_answer_only(&self, f: &Task, args: &Value) -> Result<Value, ToolError> {
        let Some(on) = args.get("on").and_then(Value::as_bool) else {
            return Err(fail("invalid", "on is true or false"));
        };
        let d = &self.inner.daemon;
        if on {
            d.can_answer_only().map_err(|e| {
                fail("not_allowed_in_this_turn", e.message)
                    .next("in Settings, run the coding agent through the Claude Agent SDK worker")
            })?;
        }
        let origin = &self.inner.origin;
        let command = origin.command("set_answer_only");
        self.record(
            TaskEvent::ToolCalled {
                tool: "set_answer_only".into(),
                origin: origin.name().into(),
                authority: origin.authority().map(|m| m.id.to_string()),
                detail: None,
            },
            Some(&command),
        )
        .await;
        let applied = d.tasks.lock().await.apply(&f.id, Some(&command), |f, _| {
            f.answer_only = on;
            // Changes are fine: a plan turned down needn't be answered in
            // words any more (D-158).
            if !on {
                crate::controller::forget_decline(f);
            }
            let why = f.reason(if on {
                "You said only an answer is wanted: the coding agent only reads."
            } else {
                "You said changes are fine: the task is no longer answer-only."
            });
            Ok(vec![
                Change::new(why),
                Change::new(TaskEvent::ToolDone {
                    tool: "set_answer_only".into(),
                    error: None,
                })
                .because(&command),
            ])
        })?;
        d.task_changed(&applied);
        let did = if on { "answer only" } else { "changes allowed" };
        if !applied.duplicate {
            self.inner
                .done
                .lock()
                .unwrap()
                .push(format!("set_answer_only: {did}"));
        }
        Ok(json!({"done": did, "status": applied.task.status.as_str()}))
    }

    /// `watch` (D-183): set on the developer's word, under their message
    /// and what is watched — a turn run again sets it once; the same thing
    /// already watched is that watch. It looks once first (W-1): the answer
    /// says what is true now, and what has already happened sets nothing.
    async fn set_watch(&self, f: &Task, args: &Value) -> Result<Value, ToolError> {
        // A watch's own turn sets no other (D-183): a watch that sets the
        // next would look again for ever, a model turn each time. Any other
        // turn may watch where that is automatic (D-190).
        if let Origin::Event { event_id } = &self.inner.origin
            && event_id.starts_with("watch:")
        {
            return Err(fail(
                "not_allowed_in_this_turn",
                "a watch's own turn doesn't set another watch",
            )
            .next("say what you'd watch; the developer can ask for it"));
        }
        let kind_name = args["kind"].as_str().unwrap_or_default();
        let kind = crate::watches::kind(kind_name).ok_or_else(|| {
            fail(
                "invalid",
                format!("there is no kind of watch `{kind_name}`"),
            )
        })?;
        let what = args["what"].as_str().unwrap_or_default().to_owned();
        let target = kind
            .target(f, args, &what)
            .map_err(|e| fail("invalid", e))?;
        // What it last read, if anything: the turn says that, not an older
        // gate (W-1).
        let shown = |w: &otter_core::watch::Watch, done: &str| {
            json!({
                "done": done,
                "watch_id": w.id.as_str(),
                "watching": w.label,
                "status": format!("{:?}", w.status).to_lowercase(),
                "until": w.deadline,
                "now": w.last_reading().map(|r| json!({"text": redact(&r.text), "read_at": r.at})),
            })
        };
        if let Some(w) = crate::watches::same(f, kind.name(), &target.value) {
            return Ok(shown(w, "already watching"));
        }
        // Refused before it reads anything: a read takes seconds of the turn.
        crate::watches::check(f, kind, args, &what).map_err(|e| fail("conflict", e))?;
        let d = &self.inner.daemon;
        let mut not_read = None;
        let first = match d.first_look(kind, f, &target).await {
            Some(crate::watches::Probe::Found { outcome, text }) => {
                self.record_done("watch", None).await;
                return Ok(json!({
                    "done": "nothing to wait for",
                    "watching": target.label,
                    "came_to": {"outcome": outcome.as_str(), "text": redact(&text)},
                    "read_at": chrono::Utc::now(),
                }));
            }
            Some(crate::watches::Probe::NotYet { key, text }) => Some(otter_core::watch::Reading {
                at: chrono::Utc::now(),
                key,
                text,
            }),
            // Set all the same, and probed at the next tick as usual.
            Some(crate::watches::Probe::Unreadable(why)) => {
                not_read = Some(redact(&why));
                None
            }
            None => None,
        };
        let origin = &self.inner.origin;
        let asked_in = origin.authority().map(|m| m.id.clone());
        let command = format!(
            "{}:watch:{}:{}",
            asked_in.as_ref().map_or("turn", |m| m.as_str()),
            kind.name(),
            target.key
        );
        self.record(
            TaskEvent::ToolCalled {
                tool: "watch".into(),
                origin: origin.name().into(),
                authority: asked_in.as_ref().map(|m| m.to_string()),
                detail: None,
            },
            Some(&command),
        )
        .await;
        let value = target.value.clone();
        let read_first = first.is_some();
        let mut set = None;
        let mut existed = false;
        let applied = d
            .tasks
            .lock()
            .await
            .apply(&f.id, Some(&command), |f, now| {
                let (w, mut changes) =
                    crate::watches::set(f, kind, target, args, &what, asked_in.clone(), first, now)
                        .map_err(RpcError::conflict)?;
                // Set meanwhile (accepting anyway): that one.
                existed = changes.is_empty();
                set = Some(w);
                changes.push(
                    Change::new(TaskEvent::ToolDone {
                        tool: "watch".into(),
                        error: None,
                    })
                    .because(&command),
                );
                Ok(changes)
            })?;
        d.task_changed(&applied);
        let watch = match set {
            Some(w) => w,
            // Set before (the turn ran again): that one.
            None => applied
                .task
                .watches
                .iter()
                .rev()
                .find(|w| w.kind == kind.name() && w.target == value)
                .cloned()
                .ok_or_else(|| fail("retryable", "the watch isn't there"))?,
        };
        if existed {
            return Ok(shown(&watch, "already watching"));
        }
        if read_first && !applied.duplicate {
            d.looked(&watch.id, kind);
        }
        self.inner
            .done
            .lock()
            .unwrap()
            .push(format!("watch: {}", watch.label));
        let mut answer = shown(
            &watch,
            if applied.duplicate {
                "already"
            } else {
                "watching"
            },
        );
        if let Some(why) = not_read {
            answer["not_read"] = json!(why);
        }
        Ok(answer)
    }

    /// `cancel_watch`, on the developer's word. Never a watch being said:
    /// its report isn't Otter's to drop.
    async fn cancel_watch(&self, f: &Task, args: &Value) -> Result<Value, ToolError> {
        let id = otter_core::WatchId::from(args["watch_id"].as_str().unwrap_or_default());
        match f.watch(&id).map(|w| w.status) {
            None => {
                return Err(fail("not_found", format!("no watch `{id}` on this task"))
                    .next("get_task lists its watches"));
            }
            Some(otter_core::watch::WatchStatus::Firing) => {
                return Err(fail(
                    "not_ready",
                    "that watch came to something: it's being said now",
                ));
            }
            _ => {}
        }
        let command = self.inner.origin.command(&format!("cancel_watch:{id}"));
        let d = &self.inner.daemon;
        let applied = d
            .tasks
            .lock()
            .await
            .apply(&f.id, Some(&command), |f, now| {
                let mut changes = crate::watches::cancel(f, &id, &command, now)?;
                changes.push(
                    Change::new(TaskEvent::ToolDone {
                        tool: "cancel_watch".into(),
                        error: None,
                    })
                    .because(&command),
                );
                Ok(changes)
            })?;
        d.task_changed(&applied);
        self.inner
            .done
            .lock()
            .unwrap()
            .push(format!("cancel_watch: {id}"));
        Ok(json!({"done": "stopped", "watch_id": id.as_str()}))
    }

    async fn write(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
        let refused = |e: ToolError| async move {
            self.record_done(name, Some(e.code)).await;
            Err(e)
        };
        let f = self.load().await?;
        if f.status.is_terminal() {
            return refused(fail("not_ready", "the task is cancelled")).await;
        }
        if name == "ask_agent" {
            return match self.ask(&f, args).await {
                Err(e) => refused(e).await,
                ok => ok,
            };
        }
        if name == "set_workspace" {
            return match self.set_workspace(&f, args).await {
                Err(e) => refused(e).await,
                ok => ok,
            };
        }
        if name == "set_answer_only" {
            return match self.set_answer_only(&f, args).await {
                Err(e) => refused(e).await,
                ok => ok,
            };
        }
        if name == "watch" || name == "cancel_watch" {
            let done = if name == "watch" {
                self.set_watch(&f, args).await
            } else {
                self.cancel_watch(&f, args).await
            };
            return match done {
                Err(e) => refused(e).await,
                ok => ok,
            };
        }
        let plan = plan_question(&f);
        let waiting_on = || {
            f.pending_decisions()
                .next()
                .map(|d| d.summary.clone())
                .unwrap_or_default()
        };
        use TaskStatus as S;
        let checked: Result<(Intent, Vec<Message>), ToolError> = match (name, f.status) {
            ("forward_to_agent", _) => self.chosen(args).map(|m| (Intent::Guide, m)),
            ("redirect_agent", _) => self.chosen(args).map(|m| (Intent::Redirect, m)),
            ("start_task", S::Draft | S::Done) => Ok((Intent::Continue, vec![])),
            ("start_task", _) => Err(fail("not_ready", "it has started already")),
            ("resume_task", S::Paused | S::Done) => Ok((Intent::Continue, vec![])),
            ("resume_task", S::Blocked) if plan.is_some() => Err(fail(
                "needs_developer",
                "it waits on the developer's approval of the plan",
            )
            .next("approve_plan, if the developer said to go ahead")),
            ("resume_task", S::Blocked) if f.pending_decisions().next().is_some() => Err(fail(
                "needs_developer",
                format!("it waits on the developer: {}", waiting_on()),
            )
            .next("the developer answers it in the task")),
            ("resume_task", S::Blocked) => Ok((Intent::Continue, vec![])),
            ("resume_task", _) => Err(fail("not_ready", "it isn't paused")),
            ("retry_task", S::Failed) => Ok((Intent::Continue, vec![])),
            ("retry_task", _) => Err(fail("not_ready", "it hasn't failed")),
            ("approve_plan", _) if plan.is_some() => Ok((Intent::Continue, vec![])),
            ("approve_plan", _) => Err(fail("not_ready", "no plan waits for approval")),
            ("pause_task", s) if s.is_active() || s == S::Blocked || s == S::Review => {
                Ok((Intent::Pause, vec![]))
            }
            ("pause_task", _) => Err(fail("not_ready", "nothing is at work")),
            ("revise_task", _) => Ok((Intent::Revise, vec![])),
            ("accept_task", S::Review) => Ok((Intent::Finish, vec![])),
            ("accept_task", _) => Err(fail("not_ready", "it isn't in review")),
            _ => Err(fail("not_found", format!("there is no tool `{name}`"))),
        };
        let (intent, messages) = match checked {
            Ok(x) => x,
            Err(e) => return refused(e).await,
        };
        let effective = effective_intent(intent, f.status, plan.is_some());
        let origin = &self.inner.origin;
        let command = origin.command(name);
        let authority = origin.authority().map(|m| m.id.to_string());
        let by_otter = authority.is_none();
        // Written before the change: a change that never finished is known.
        self.record(
            TaskEvent::ToolCalled {
                tool: name.into(),
                origin: origin.name().into(),
                authority: authority.clone(),
                detail: None,
            },
            Some(&command),
        )
        .await;
        crash_point("called", name);
        let d = &self.inner.daemon;
        let mut stop = false;
        let mut not_done: Option<String> = None;
        // A plan question already put to the developer is theirs: Otter
        // approves plans on its own before asking (D-192), not over them.
        if by_otter && name == "approve_plan" {
            let e = fail(
                "needs_developer",
                "the plan was put to the developer, and only they can answer that question now",
            )
            .next("tell the developer what you'd say");
            self.record_done(name, Some(e.code)).await;
            return Err(e);
        }
        let applied = d.tasks.lock().await.apply(&f.id, Some(&command), |f, now| {
            // Without the developer's word (an event's turn, D-192), it is
            // Otter's own doing, and the record says so.
            let (mut changes, stops) = intent_step(
                f,
                effective,
                authority.as_deref().unwrap_or(&command),
                by_otter,
                now,
            );
            stop = stops;
            if effective == Intent::Finish && f.status != TaskStatus::Done {
                not_done = Some(
                    f.rationale
                        .last()
                        .cloned()
                        .unwrap_or_else(|| "it wasn't accepted".into()),
                );
            }
            changes.push(
                Change::new(TaskEvent::ToolDone {
                    tool: name.into(),
                    error: not_done.as_ref().map(|_| "not_ready".into()),
                })
                .because(&command),
            );
            Ok(changes)
        });
        let applied = match applied {
            Ok(a) => a,
            Err(e) => {
                let e = ToolError::from(e);
                self.record_done(name, Some(e.code)).await;
                return Err(e);
            }
        };
        d.task_changed(&applied);
        crash_point("applied", name);
        let redirect: String = messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if applied.duplicate {
            // Applied before (the turn ran again): what reaches the agent is
            // queued once by message id, so passing it on again is safe.
            if matches!(effective, Intent::Guide | Intent::Redirect) {
                d.after_intent(&f, effective, &messages, &redirect, false, None)
                    .await?;
            }
            return Ok(json!({"done": "already", "status": applied.task.status.as_str()}));
        }
        let approve = (name == "approve_plan").then_some(plan).flatten();
        d.after_intent(&f, effective, &messages, &redirect, stop, approve)
            .await?;
        if let Some(why) = not_done {
            return Err(fail("not_ready", why).next("say what is missing"));
        }
        let did = match effective {
            Intent::Guide => "passed on",
            Intent::Redirect => "redirected",
            Intent::Continue if name == "approve_plan" => "plan approved",
            Intent::Continue => "going on",
            Intent::Pause => "paused",
            Intent::Revise => "planning again",
            Intent::Finish => "accepted",
            Intent::None => "nothing",
        };
        self.inner
            .done
            .lock()
            .unwrap()
            .push(format!("{name}: {did}"));
        let status = d.task_get(f.id.as_str()).await?.status;
        Ok(json!({"done": did, "status": status.as_str()}))
    }
}

/// The most one note in Otter's memory says, in characters.
const MAX_NOTE: usize = 1_000;

/// Whether `memory` already says `note`: a line the same but for its
/// bullet (a note of several lines: all of it, as written).
fn noted(memory: &str, note: &str) -> bool {
    let bare = |l: &str| l.trim().trim_start_matches(['-', '*']).trim().to_owned();
    if note.trim().contains('\n') {
        return memory.contains(note.trim());
    }
    memory.lines().any(|l| bare(l) == bare(note))
}

/// `get_messages`: a conversation's messages, newest first and whole (but
/// redacted, and bounded: a message to 8 000 characters, a page to 40 000).
fn messages_page(all: &[Message], args: &Value) -> Result<Value, ToolError> {
    let limit = match args.get("limit") {
        None | Some(Value::Null) => 10,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=30).contains(n))
            .ok_or_else(|| fail("invalid", "limit is a number from 1 to 30"))?
            as usize,
    };
    let end = match args.get("before").and_then(Value::as_str) {
        None | Some("") => all.len(),
        Some(id) => all
            .iter()
            .position(|m| m.id.as_str() == id)
            .ok_or_else(|| {
                fail(
                    "invalid",
                    format!("{id} isn't a message in this conversation"),
                )
            })?,
    };
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(|q| q.trim().to_lowercase())
        .filter(|q| !q.is_empty());
    let mut page = Vec::new();
    let mut size = 0;
    let mut more = false;
    for m in all[..end].iter().rev() {
        if let Some(q) = &query
            && !m.text.to_lowercase().contains(q)
        {
            continue;
        }
        if page.len() == limit || (size >= 40_000 && !page.is_empty()) {
            more = true;
            break;
        }
        let text = cut(&redact(&m.text), 8_000);
        size += text.len();
        page.push(json!({
            "id": m.id.as_str(),
            "from": match m.role {
                otter_core::task::MessageRole::User => "developer",
                otter_core::task::MessageRole::Controller => "otter",
                otter_core::task::MessageRole::Agent => "coding agent",
                otter_core::task::MessageRole::System => "system",
            },
            "at": m.at,
            "text": text,
        }));
    }
    Ok(json!({"newest_first": page, "more": more, "in_conversation": all.len()}))
}

/// `text` to at most `max` characters, its lines kept.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Tests only (`OTTER_TEST_CRASH=<point>:<tool>`): otterd dies here, as a
/// crash would — `called` once a change is written down, `applied` once it
/// is made and before anything reaches the agent — to show what the next
/// start makes of it (plan §8).
pub(crate) fn crash_point(point: &str, tool: &str) {
    if std::env::var("OTTER_TEST_CRASH").is_ok_and(|v| v == format!("{point}:{tool}")) {
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_tool_has_one_class_and_a_closed_schema() {
        for all in [tools(), host_tools()] {
            let mut names: Vec<&str> = all.iter().map(|t| t.name).collect();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), all.len(), "a name twice");
            for t in &all {
                assert_eq!(t.input["type"], "object", "{}", t.name);
                assert_eq!(t.input["additionalProperties"], false, "{}", t.name);
            }
            // Only pausing changes anything without the developer's word.
            let routine: Vec<&str> = all
                .iter()
                .filter(|t| t.class == Class::Routine)
                .map(|t| t.name)
                .collect();
            assert_eq!(routine, ["pause_task"]);
            // Never Otter's: cancel, delete, decide anything else, publish.
            for n in [
                "cancel_task",
                "delete_task",
                "decide",
                "answer_pending_decision",
                "publish",
            ] {
                assert!(!names.contains(&n), "{n}");
            }
            // Every conversation reads its messages and keeps a memory, which
            // only a turn the developer started changes (D-113).
            let class = |n: &str| all.iter().find(|t| t.name == n).unwrap().class;
            assert_eq!(class("get_messages"), Class::Observe);
            assert_eq!(class("remember"), Class::DeveloperWord);
            assert_eq!(class("edit_memory"), Class::DeveloperWord);
        }
        let all = tools();
        let class = |n: &str| all.iter().find(|t| t.name == n).unwrap().class;
        for n in [
            "forward_to_agent",
            "ask_agent",
            "redirect_agent",
            "start_task",
            "resume_task",
            "retry_task",
            "approve_plan",
            "revise_task",
            "set_answer_only",
            "accept_task",
        ] {
            assert_eq!(class(n), Class::DeveloperWord, "{n}");
        }
    }

    fn msg(id: &str, role: otter_core::task::MessageRole, text: &str) -> Message {
        Message {
            id: otter_core::MessageId::from(id),
            role,
            text: text.into(),
            at: chrono::Utc::now(),
            correlation_id: None,
            attachments: vec![],
        }
    }

    #[test]
    fn a_note_already_in_memory_is_one_line_the_same() {
        let memory = "- Use Postgres 16\n* small PRs\n## Auth\n- tokens expire in 1h\n";
        assert!(noted(memory, "Use Postgres 16"));
        assert!(noted(memory, "- small PRs"));
        assert!(
            !noted(memory, "Use Postgres"),
            "part of a line isn't the note"
        );
        assert!(!noted(memory, "PRs"));
        assert!(noted(memory, "## Auth\n- tokens expire in 1h"));
        assert!(!noted("", "anything"));
    }

    #[test]
    fn older_messages_come_whole_newest_first_a_page_at_a_time() {
        use otter_core::task::MessageRole::{Controller, User};
        let all: Vec<Message> = (1..=25)
            .map(|i| {
                let role = if i % 2 == 1 { User } else { Controller };
                msg(
                    &format!("msg_{i}"),
                    role,
                    &format!("line one {i}\nline two"),
                )
            })
            .collect();
        let ids = |v: &Value| -> Vec<String> {
            v["newest_first"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let first = messages_page(&all, &json!({})).unwrap();
        assert_eq!(ids(&first)[..2], ["msg_25", "msg_24"]);
        assert_eq!(ids(&first).len(), 10);
        assert_eq!(first["more"], true);
        assert_eq!(first["in_conversation"], 25);
        // Whole: lines kept.
        assert_eq!(first["newest_first"][0]["text"], "line one 25\nline two");
        assert_eq!(first["newest_first"][0]["from"], "developer");
        assert_eq!(first["newest_first"][1]["from"], "otter");
        let back = messages_page(&all, &json!({"before": "msg_6", "limit": 30})).unwrap();
        assert_eq!(ids(&back), ["msg_5", "msg_4", "msg_3", "msg_2", "msg_1"]);
        assert_eq!(back["more"], false);
        let found = messages_page(&all, &json!({"query": "ONE 1"})).unwrap();
        assert_eq!(
            ids(&found),
            [
                "msg_19", "msg_18", "msg_17", "msg_16", "msg_15", "msg_14", "msg_13", "msg_12",
                "msg_11", "msg_10"
            ]
        );
        assert_eq!(found["more"], true, "msg_1 matches too");
        assert!(messages_page(&all, &json!({"before": "msg_x"})).is_err());
        assert!(messages_page(&all, &json!({"limit": 31})).is_err());
        // Redacted, and bounded.
        let secret = vec![msg(
            "msg_s",
            User,
            "ANTHROPIC_API_KEY=sk-ant-abc123def456ghi",
        )];
        let v = messages_page(&secret, &json!({})).unwrap();
        assert!(!v.to_string().contains("sk-ant-abc123def456ghi"), "{v}");
        let big: Vec<Message> = (0..10)
            .map(|i| msg(&format!("msg_b{i}"), User, &"x".repeat(9_000)))
            .collect();
        let v = messages_page(&big, &json!({"limit": 30})).unwrap();
        assert_eq!(
            v["newest_first"][0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            8_000
        );
        assert_eq!(ids(&v).len(), 5, "a page stops at 40 000 characters");
        assert_eq!(v["more"], true);
    }

    #[test]
    fn a_change_is_applied_under_the_developers_message() {
        let m = Message {
            id: otter_core::MessageId::from("msg_1"),
            role: otter_core::task::MessageRole::User,
            text: "ship it".into(),
            at: chrono::Utc::now(),
            correlation_id: None,
            attachments: vec![],
        };
        let dev = Origin::Developer { messages: vec![m] };
        assert_eq!(dev.command("accept_task"), "msg_1:accept_task");
        assert_eq!(dev.authority().unwrap().id.as_str(), "msg_1");
        let ev = Origin::Event {
            event_id: "ev_9".into(),
        };
        assert_eq!(ev.command("pause_task"), "ev_9:pause_task");
        assert!(ev.authority().is_none());
    }

    #[test]
    fn errors_say_what_and_what_next_without_secrets() {
        let e = fail(
            "needs_developer",
            "waits on ANTHROPIC_API_KEY=sk-ant-abc123def456",
        )
        .next("the developer answers it");
        let v = answer(&Err(e));
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "needs_developer");
        assert_eq!(v["error"]["next"], "the developer answers it");
        assert!(!v.to_string().contains("sk-ant-abc123def456"), "{v}");
        assert_eq!(answer(&Ok(json!({"x": 1})))["data"]["x"], 1);
    }
}
