//! Otter's tools (D-089): what a turn may look at and change, and the
//! checks — here, in otterd, never in the model.
//!
//! - **Observe** tools read the feature, its history, its run, its
//!   decisions and evidence; everything they return is redacted (D-075) and
//!   bounded.
//! - **Routine**: pausing — the safe direction — in any turn.
//! - **On the developer's word**: forwarding their messages, redirecting,
//!   starting, resuming, retrying, approving the plan, accepting, a changed
//!   goal. Only in a turn the developer's message started; each change is
//!   applied once under `<that message>:<tool>` (forwarding keeps each
//!   message's own id, D-059).
//!
//! Every change goes through what `converse` does (`effective_intent`,
//! `intent_step`, `after_intent`): a tool and a classified message have the
//! same effect. A change is written to the feature's history before it is
//! made (`ToolCalled`), and what came of it with it (`ToolDone`). A tool,
//! once called, finishes even when the turn stops waiting for it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use otter_core::FeatureId;
use otter_core::feature::{Feature, FeatureEvent, FeatureStatus, Message};
use otter_core::redact::redact;
use otter_protocol::{ErrorCode, RpcError};
use serde_json::{Value, json};

use crate::agents::excerpt;
use crate::brain::Intent;
use crate::controller::{effective_intent, intent_step, plan_question};
use crate::daemon::Daemon;
use crate::features::Change;

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
            "accept_task",
            DeveloperWord,
            "Accept the task in review; its gates still apply.",
            none(),
        ),
    ]
}

/// Why a tool did nothing, and what would work instead.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    pub next: Option<String>,
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

/// One turn's tools: its origin, its feature, how many calls it has left.
#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

struct Inner {
    daemon: Arc<Daemon>,
    feature: FeatureId,
    origin: Origin,
    max_calls: usize,
    calls: AtomicUsize,
    /// What this turn changed, a line each.
    done: Mutex<Vec<String>>,
}

impl Gateway {
    pub fn new(daemon: Arc<Daemon>, feature: FeatureId, origin: Origin, max_calls: usize) -> Self {
        Gateway {
            inner: Arc::new(Inner {
                daemon,
                feature,
                origin,
                max_calls,
                calls: AtomicUsize::new(0),
                done: Mutex::new(Vec::new()),
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
            tracing::debug!(feature = %self.inner.feature, tool = name, code = e.code, "Otter's tool");
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
        let Some(spec) = tools().into_iter().find(|t| t.name == name) else {
            return Err(fail("not_found", format!("there is no tool `{name}`")));
        };
        if spec.class == Class::DeveloperWord && self.inner.origin.authority().is_none() {
            let e = fail(
                "not_allowed_in_this_turn",
                format!("`{name}` acts on the developer's word, and no message of theirs started this turn"),
            )
            .next("tell the developer what you would do; they can ask for it");
            self.record_done(name, Some(e.code)).await;
            return Err(e);
        }
        let result = match spec.class {
            Class::Observe => self.observe(name, args).await,
            Class::Routine | Class::DeveloperWord => self.write(name, args).await,
        };
        if spec.class == Class::Observe {
            self.record_done(name, result.as_ref().err().map(|e| e.code))
                .await;
        }
        result
    }

    async fn feature(&self) -> Result<Feature, ToolError> {
        Ok(self
            .inner
            .daemon
            .feature_get(self.inner.feature.as_str())
            .await?)
    }

    /// Write `ToolDone` for a call that changed nothing (a read, a refusal).
    async fn record_done(&self, tool: &str, error: Option<&str>) {
        let event = FeatureEvent::ToolDone {
            tool: tool.into(),
            error: error.map(Into::into),
        };
        self.record(event, None).await;
    }

    async fn record(&self, event: FeatureEvent, because: Option<&str>) {
        let d = &self.inner.daemon;
        let applied = d
            .features
            .lock()
            .await
            .apply(&self.inner.feature, None, |_, _| {
                let c = Change::new(event);
                Ok(vec![match because {
                    Some(id) => c.because(id),
                    None => c,
                }])
            });
        match applied {
            Ok(a) => d.feature_changed(&a),
            Err(e) => {
                tracing::warn!(feature = %self.inner.feature, "recording Otter's tool: {}", e.message)
            }
        }
    }

    // --- Observe -----------------------------------------------------------

    async fn observe(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
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
        // A live run's activity isn't in the feature until it ends.
        let f = self.inner.daemon.with_live_activity(self.feature().await?);
        let r = |s: &str| redact(s);
        let short = |s: &str, n: usize| excerpt(&redact(s), n);
        match name {
            "get_task" => Ok(json!({
                "id": f.id.as_str(),
                "title": r(&f.title),
                "request": short(&f.request, 2000),
                "status": f.status.as_str(),
                "status_reason": f.status_reason.as_deref().map(r),
                "steps": f.tasks.iter().map(|t| json!({
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
            })),
            "get_task_history" => {
                let n = lim(20, 100)?;
                let all = self
                    .inner
                    .daemon
                    .features
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
                let step = f.tasks.iter().find(|t| t.id == run.task_id);
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

    // --- Changes -----------------------------------------------------------

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
    async fn ask(&self, f: &Feature, args: &Value) -> Result<Value, ToolError> {
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
            FeatureEvent::ToolCalled {
                tool: "ask_agent".into(),
                origin: origin.name().into(),
                authority: origin.authority().map(|m| m.id.to_string()),
            },
            Some(&command),
        )
        .await;
        let applied = d
            .features
            .lock()
            .await
            .apply(&f.id, Some(&command), |_, _| {
                Ok(vec![
                    Change::new(FeatureEvent::ToolDone {
                        tool: "ask_agent".into(),
                        error: None,
                    })
                    .because(&command),
                ])
            })?;
        d.feature_changed(&applied);
        // Each message once, by its own id (D-059): asked again (a turn run
        // again), it isn't queued twice.
        let carried = d.carried(f);
        let mut asked = 0;
        for m in messages.iter().filter(|m| !carried.contains(m.id.as_str())) {
            asked += d.ask_runs(&f.id, m.id.as_str(), &m.text).await;
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

    async fn write(&self, name: &str, args: &Value) -> Result<Value, ToolError> {
        let refused = |e: ToolError| async move {
            self.record_done(name, Some(e.code)).await;
            Err(e)
        };
        let f = self.feature().await?;
        if f.status.is_terminal() {
            return refused(fail("not_ready", "the task is cancelled")).await;
        }
        if name == "ask_agent" {
            return match self.ask(&f, args).await {
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
        use FeatureStatus as S;
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
            FeatureEvent::ToolCalled {
                tool: name.into(),
                origin: origin.name().into(),
                authority: authority.clone(),
            },
            Some(&command),
        )
        .await;
        crash_point("called", name);
        let d = &self.inner.daemon;
        let mut stop = false;
        let mut not_done: Option<String> = None;
        let applied = d
            .features
            .lock()
            .await
            .apply(&f.id, Some(&command), |f, now| {
                let (mut changes, stops) = if by_otter {
                    // Only pausing gets here without the developer's word.
                    let c = f
                        .transition(FeatureStatus::Paused, Some("Paused by Otter".into()), now)
                        .map_err(RpcError::conflict)?;
                    (vec![Change::new(c)], true)
                } else {
                    intent_step(f, effective, authority.as_deref().unwrap_or(&command), now)
                };
                stop = stops;
                if effective == Intent::Finish && f.status != FeatureStatus::Done {
                    not_done = Some(
                        f.rationale
                            .last()
                            .cloned()
                            .unwrap_or_else(|| "it wasn't accepted".into()),
                    );
                }
                changes.push(
                    Change::new(FeatureEvent::ToolDone {
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
        d.feature_changed(&applied);
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
            return Ok(json!({"done": "already", "status": applied.feature.status.as_str()}));
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
        let status = d.feature_get(f.id.as_str()).await?.status;
        Ok(json!({"done": did, "status": status.as_str()}))
    }
}

/// Tests only (`OTTER_TEST_CRASH=<point>:<tool>`): otterd dies here, as a
/// crash would — `called` once a change is written down, `applied` once it
/// is made and before anything reaches the agent — to show what the next
/// start makes of it (plan §8).
fn crash_point(point: &str, tool: &str) {
    if std::env::var("OTTER_TEST_CRASH").is_ok_and(|v| v == format!("{point}:{tool}")) {
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_tool_has_one_class_and_a_closed_schema() {
        let all = tools();
        let mut names: Vec<&str> = all.iter().map(|t| t.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), all.len(), "a name twice");
        for t in &all {
            assert_eq!(t.input["type"], "object", "{}", t.name);
            assert_eq!(t.input["additionalProperties"], false, "{}", t.name);
        }
        let class = |n: &str| all.iter().find(|t| t.name == n).unwrap().class;
        // Only pausing changes anything without the developer's word.
        let routine: Vec<&str> = all
            .iter()
            .filter(|t| t.class == Class::Routine)
            .map(|t| t.name)
            .collect();
        assert_eq!(routine, ["pause_task"]);
        for n in [
            "forward_to_agent",
            "ask_agent",
            "redirect_agent",
            "start_task",
            "resume_task",
            "retry_task",
            "approve_plan",
            "revise_task",
            "accept_task",
        ] {
            assert_eq!(class(n), Class::DeveloperWord, "{n}");
        }
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
    }

    #[test]
    fn a_change_is_applied_under_the_developers_message() {
        let m = Message {
            id: otter_core::MessageId::from("msg_1"),
            role: otter_core::feature::MessageRole::User,
            text: "ship it".into(),
            at: chrono::Utc::now(),
            correlation_id: None,
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
