//! The Control Agent's judgment (D-045): the few places a model decides —
//! writing the plan, deciding what policy leaves open, judging the evidence.
//! Everything else (lifecycle, limits, retries, what may run) is the
//! controller's deterministic code.
//!
//! `OTTER_CONTROLLER` (or Settings) picks the brain: a model provider
//! (`openrouter`, `anthropic`, `openai`, … — see `providers.rs`; by default
//! the first one with a key), `claude` (`claude -p` with structured output,
//! no tools; the default otherwise, when installed), `rules` (no model: one
//! task, every open decision goes to the developer, criteria are met when
//! the check passes), or `yes` (tests only: approves everything it is asked
//! — to show that policy still stops it). `OTTER_CONTROLLER_MODEL` picks the
//! model for any of them.

use std::path::Path;

use anyhow::{Result, bail};
use async_trait::async_trait;
use otter_core::feature::{DecisionKind, DecisionRequest, Feature};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::env::EnvMap;
use crate::providers::Provider;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedTask {
    pub title: String,
    #[serde(default)]
    pub detail: String,
    /// Indexes of tasks (in this plan) that must be done first.
    #[serde(default)]
    pub depends_on: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub requirements: Vec<String>,
    pub criteria: Vec<String>,
    pub tasks: Vec<PlannedTask>,
    /// One shell command that checks the work, if the project has one.
    #[serde(default)]
    pub verify_command: Option<String>,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Allow {
        rationale: String,
    },
    Deny {
        rationale: String,
    },
    Answer {
        text: String,
        rationale: String,
    },
    /// Not the Control Agent's call: ask the developer.
    Escalate {
        why: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CriterionJudgement {
    pub id: String,
    pub met: bool,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Judgement {
    pub criteria: Vec<CriterionJudgement>,
    /// What to fix when something isn't met.
    #[serde(default)]
    pub remediation: String,
}

/// What a brain can look at: the feature and the workspace it works in.
pub struct Context<'a> {
    pub feature: &'a Feature,
    pub root: &'a Path,
    pub env: &'a EnvMap,
}

#[async_trait]
pub trait Brain: Send + Sync {
    fn name(&self) -> &'static str;
    async fn plan(&self, cx: &Context<'_>) -> Result<Plan>;
    async fn decide(&self, cx: &Context<'_>, d: &DecisionRequest) -> Result<Verdict>;
    async fn judge(&self, cx: &Context<'_>) -> Result<Judgement>;
    /// Look at a screenshot of `check` and say whether the feature looks
    /// right: supplementary, uncertain evidence. `None`: no opinion.
    async fn review(
        &self,
        _cx: &Context<'_>,
        _check: &str,
        _screenshot: &Path,
    ) -> Result<Option<(bool, String)>> {
        Ok(None)
    }
    /// Answer the developer's message (D-049): what's going on, and whether
    /// they asked to go on or to stop — a model's judgment. Without a model:
    /// the status only; acting on the message is left to the buttons.
    async fn reply(&self, cx: &Context<'_>, _message: &str, _say: &Say<'_>) -> Result<Reply> {
        Ok(Reply {
            text: format!(
                "{} (No model is set for the Control Agent on this host, so I can't act on messages: use the buttons, or choose a model in Settings.)",
                status_text(cx.feature)
            ),
            intent: Intent::None,
        })
    }
}

/// Where a reply's text goes as it is written: each piece, in order.
pub type Say<'a> = dyn Fn(&str) + Send + Sync + 'a;

/// What the developer's message asks the feature to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// Just talking (or instructions for the coding agent).
    None,
    /// Go on: resume, unblock, start, or retry.
    Continue,
    Pause,
    /// The goal changed or grew: plan again from where the work stands.
    Revise,
    /// They're satisfied: accept it (in review), or stop here.
    Finish,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub text: String,
    pub intent: Intent,
}

/// Where the feature stands, in a few lines (deterministic).
pub fn status_text(f: &Feature) -> String {
    use otter_core::feature::{FeatureStatus as S, TaskStatus};
    let done = f
        .tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Done | TaskStatus::Skipped))
        .count();
    let current = f
        .tasks
        .iter()
        .position(|t| t.status == TaskStatus::Running)
        .map(|i| (i, &f.tasks[i]));
    let mut lines = vec![match f.status {
        S::Draft => "This is a draft: say “start” (or press Start) and I'll plan it.".to_owned(),
        S::Planning => "I'm planning: requirements, acceptance criteria and tasks.".to_owned(),
        S::Implementing => match current {
            Some((i, t)) => format!(
                "A coding agent is working on task {} of {}: “{}”{}.",
                i + 1,
                f.tasks.len(),
                t.title,
                if t.attempts > 1 {
                    format!(" (attempt {})", t.attempts)
                } else {
                    String::new()
                }
            ),
            None => format!(
                "Implementing: {done} of {} tasks done; the next one starts shortly.",
                f.tasks.len()
            ),
        },
        S::Verifying => "I'm checking the work against the acceptance criteria.".to_owned(),
        S::Review => {
            "It's verified and waiting for your review (see Delivery for the gates).".to_owned()
        }
        S::Done => "It's done.".to_owned(),
        S::Blocked => format!(
            "I'm blocked: {}.",
            f.status_reason
                .clone()
                .unwrap_or_else(|| "waiting on you".into())
        ),
        S::Paused => "It's paused. Say “continue” to pick it up again.".to_owned(),
        S::Failed => format!(
            "It stopped: {}. Say “continue” to retry from the plan.",
            f.status_reason
                .clone()
                .unwrap_or_else(|| "it failed".into())
        ),
        S::Cancelled => "It's cancelled.".to_owned(),
    }];
    let pending: Vec<&str> = f.pending_decisions().map(|d| d.summary.as_str()).collect();
    if !pending.is_empty() {
        lines.push(format!(
            "Waiting for your decision: {}.",
            pending.join("; ")
        ));
    }
    if f.status == S::Implementing || f.status == S::Planning {
        lines.push("Your message goes to the coding agent with its next turn.".into());
    }
    lines.join(" ")
}

/// What the brain is chosen from: the host's settings (D-048), with an
/// explicit environment winning.
#[derive(Clone, Debug, Default)]
pub struct Choice {
    /// `claude`, a provider's id, `rules`, `yes`, `off`; `None`: automatic.
    pub controller: Option<String>,
    pub model: Option<String>,
    /// Providers' keys set in Settings, by name (otterd's environment is
    /// looked at too).
    pub keys: std::collections::BTreeMap<String, String>,
}

impl Choice {
    fn key(&self, p: &Provider, env: &EnvMap) -> Option<String> {
        self.keys.get(p.key).cloned().or_else(|| p.env_key(env))
    }
}

/// The brain chosen (automatic: the first provider with a key — the Control
/// Agent's own model, apart from the coding agent's — else Claude Code when
/// installed, else rules).
pub fn select(env: &EnvMap, choice: &Choice) -> Box<dyn Brain> {
    let model = |backend| {
        let key = match backend {
            Backend::Api(p) => choice.key(p, env),
            Backend::ClaudeCode => None,
        };
        Box::new(Model {
            backend,
            model: choice.model.clone(),
            key,
        })
    };
    match choice.controller.as_deref().unwrap_or_default() {
        "rules" | "off" => Box::new(Rules),
        "yes" => Box::new(YesMan),
        "claude" => model(Backend::ClaudeCode),
        id => match crate::providers::get(id).or_else(|| {
            crate::providers::PROVIDERS
                .iter()
                .find(|p| choice.key(p, env).is_some())
        }) {
            Some(p) => model(Backend::Api(p)),
            None if crate::env::which("claude", env).is_some() => model(Backend::ClaudeCode),
            None => Box::new(Rules),
        },
    }
}

/// The project's usual check, from the files at its root.
pub fn detect_verify_command(root: &Path) -> Option<String> {
    let has = |f: &str| root.join(f).exists();
    if has("Cargo.toml") {
        Some("cargo test".into())
    } else if has("package.json") {
        Some("npm test".into())
    } else if has("go.mod") {
        Some("go test ./...".into())
    } else if has("pyproject.toml") || has("pytest.ini") {
        Some("pytest".into())
    } else if std::fs::read_to_string(root.join("Makefile"))
        .is_ok_and(|m| m.lines().any(|l| l.starts_with("test:")))
    {
        Some("make test".into())
    } else {
        None
    }
}

fn rules_plan(cx: &Context<'_>) -> Plan {
    let f = cx.feature;
    // Planning again without a model: one task carrying what the developer
    // said since, the criteria as they were.
    if f.replan {
        let notes: Vec<String> = f
            .messages
            .iter()
            .skip(1)
            .filter(|m| m.role == otter_core::feature::MessageRole::User)
            .map(|m| m.text.clone())
            .collect();
        return Plan {
            requirements: f.requirements.clone(),
            criteria: f.acceptance.iter().map(|c| c.text.clone()).collect(),
            tasks: vec![PlannedTask {
                title: "Apply the developer's changes".into(),
                detail: notes.join("\n"),
                depends_on: vec![],
            }],
            verify_command: f
                .verify_command
                .clone()
                .or_else(|| detect_verify_command(cx.root)),
            rationale:
                "The goal changed (no model configured): one task with the developer's changes."
                    .into(),
        };
    }
    let request = if f.request.trim().is_empty() {
        f.title.clone()
    } else {
        f.request.clone()
    };
    // Bulleted lines in the request read as acceptance criteria.
    let bullets: Vec<String> = request
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")))
        .map(String::from)
        .collect();
    let verify = detect_verify_command(cx.root);
    let mut criteria = if bullets.is_empty() {
        vec![format!("Done: {}", f.title)]
    } else {
        bullets
    };
    if let Some(cmd) = &verify {
        criteria.push(format!("`{cmd}` passes"));
    }
    Plan {
        requirements: vec![request.clone()],
        criteria,
        tasks: vec![PlannedTask {
            title: f.title.clone(),
            detail: request,
            depends_on: vec![],
        }],
        verify_command: verify,
        rationale: "One task for the whole request (no model configured).".into(),
    }
}

/// Criteria are met when the latest check passed, and nothing failed since.
fn rules_judge(cx: &Context<'_>) -> Judgement {
    let f = cx.feature;
    let latest = f.evidence.iter().rev().find(|e| e.ok.is_some());
    let passed = latest.is_some_and(|e| e.ok == Some(true));
    let reason = match latest {
        Some(e) if passed => format!("{} passed", e.title),
        Some(e) => format!("{} failed", e.title),
        None => "nothing checked it automatically".into(),
    };
    Judgement {
        criteria: f
            .acceptance
            .iter()
            .map(|c| CriterionJudgement {
                id: c.id.clone(),
                met: passed,
                reason: reason.clone(),
            })
            .collect(),
        remediation: if passed {
            String::new()
        } else {
            latest
                .and_then(|e| e.detail.clone())
                .unwrap_or_else(|| reason.clone())
        },
    }
}

pub struct Rules;

#[async_trait]
impl Brain for Rules {
    fn name(&self) -> &'static str {
        "rules"
    }
    async fn plan(&self, cx: &Context<'_>) -> Result<Plan> {
        Ok(rules_plan(cx))
    }
    async fn decide(&self, _: &Context<'_>, d: &DecisionRequest) -> Result<Verdict> {
        Ok(Verdict::Escalate {
            why: format!("No model decides for you here: {}", d.summary),
        })
    }
    async fn judge(&self, cx: &Context<'_>) -> Result<Judgement> {
        Ok(rules_judge(cx))
    }
}

/// Approves whatever it is asked. Only for tests: policy must still stop it.
pub struct YesMan;

#[async_trait]
impl Brain for YesMan {
    fn name(&self) -> &'static str {
        "yes"
    }
    async fn plan(&self, cx: &Context<'_>) -> Result<Plan> {
        Ok(rules_plan(cx))
    }
    async fn decide(&self, _: &Context<'_>, d: &DecisionRequest) -> Result<Verdict> {
        Ok(match d.kind {
            DecisionKind::Question => Verdict::Answer {
                text: d.options.first().cloned().unwrap_or_else(|| "yes".into()),
                rationale: "yes".into(),
            },
            _ => Verdict::Allow {
                rationale: "yes".into(),
            },
        })
    }
    async fn judge(&self, cx: &Context<'_>) -> Result<Judgement> {
        Ok(rules_judge(cx))
    }
}

/// Where a model brain's answers come from.
#[derive(Clone, Copy, Debug)]
pub enum Backend {
    /// `claude -p` with a JSON schema and no tools (the developer's Claude
    /// Code sign-in).
    ClaudeCode,
    /// A provider's API, with its key.
    Api(&'static Provider),
}

/// A model deciding where judgment is needed.
pub struct Model {
    pub backend: Backend,
    /// The model to ask (`None`: the backend's default).
    pub model: Option<String>,
    /// The provider's key.
    pub key: Option<String>,
}

impl Model {
    fn key(&self, p: &Provider) -> Result<&str> {
        self.key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("no {} API key: set one in Settings on this host", p.label)
        })
    }
}

impl Model {
    /// One JSON answer fitting `schema`; `image`: a screenshot to look at.
    async fn ask(
        &self,
        cx: &Context<'_>,
        prompt: &str,
        schema: &serde_json::Value,
        image: Option<&Path>,
    ) -> Result<serde_json::Value> {
        match self.backend {
            Backend::ClaudeCode => {
                let prompt = match image {
                    Some(p) => format!("Read the screenshot {} first.\n{prompt}", p.display()),
                    None => prompt.to_owned(),
                };
                crate::agents::claude_stream::structured_reading(
                    cx.env,
                    cx.root,
                    &prompt,
                    schema,
                    image.and_then(|p| p.parent()),
                    self.model.as_deref(),
                )
                .await
            }
            Backend::Api(p) => {
                p.structured(
                    cx.env,
                    self.key(p)?,
                    self.model.as_deref(),
                    prompt,
                    schema,
                    image,
                )
                .await
            }
        }
    }
}

fn brief(f: &Feature) -> String {
    let mut s = format!("Feature: {}\nRequest:\n{}\n", f.title, f.request);
    let user: Vec<&str> = f
        .messages
        .iter()
        .filter(|m| m.role == otter_core::feature::MessageRole::User)
        .map(|m| m.text.as_str())
        .collect();
    if user.len() > 1 {
        s.push_str("\nLater messages from the developer:\n");
        for m in &user[1..] {
            s.push_str(&format!("- {m}\n"));
        }
    }
    s
}

#[async_trait]
impl Brain for Model {
    fn name(&self) -> &'static str {
        match self.backend {
            Backend::ClaudeCode => "claude",
            Backend::Api(p) => p.id,
        }
    }

    async fn plan(&self, cx: &Context<'_>) -> Result<Plan> {
        let detected = detect_verify_command(cx.root);
        // Planning again: what's done stays done; plan what's left.
        let done: Vec<String> = cx
            .feature
            .tasks
            .iter()
            .filter(|t| matches!(t.status, otter_core::feature::TaskStatus::Done))
            .map(|t| format!("- {}", t.title))
            .collect();
        let again = if cx.feature.replan && !done.is_empty() {
            format!(
                "\nThis is a revised plan: the developer changed or added to the goal (see their later messages). \
                 Already done (don't redo, don't list again):\n{}\nPlan only the remaining work; restate the \
                 requirements and criteria for the goal as it is now.\n",
                done.join("\n")
            )
        } else {
            String::new()
        };
        let prompt = format!(
            "You are the Control Agent planning a software feature for a coding agent working in {root}.\n\
             {brief}\n\
             Write: requirements (short), acceptance criteria that can be checked, and 1-5 tasks in order \
             (depends_on = indexes of earlier tasks). verify_command: one shell command that checks the work \
             (the project's tests{hint}), or null. rationale: one sentence.{again}",
            root = cx.root.display(),
            brief = brief(cx.feature),
            hint = detected
                .as_deref()
                .map(|c| format!("; this project looks like `{c}`"))
                .unwrap_or_default(),
        );
        let schema = json!({
            "type": "object",
            "properties": {
                "requirements": {"type": "array", "items": {"type": "string"}},
                "criteria": {"type": "array", "items": {"type": "string"}},
                "tasks": {"type": "array", "items": {"type": "object", "properties": {
                    "title": {"type": "string"}, "detail": {"type": "string"},
                    "depends_on": {"type": "array", "items": {"type": "integer"}}},
                    "required": ["title", "detail", "depends_on"]}},
                "verify_command": {"type": ["string", "null"]},
                "rationale": {"type": "string"}
            },
            "required": ["requirements", "criteria", "tasks", "verify_command", "rationale"]
        });
        let v = self.ask(cx, &prompt, &schema, None).await?;
        let plan: Plan = serde_json::from_value(v)?;
        if plan.tasks.is_empty() {
            bail!("the plan has no tasks");
        }
        Ok(plan)
    }

    async fn decide(&self, cx: &Context<'_>, d: &DecisionRequest) -> Result<Verdict> {
        let prompt = format!(
            "You are the Control Agent supervising a coding agent on this feature.\n{brief}\n\
             The agent asks ({kind:?}, {risk:?} risk): {summary}\nPolicy note: {detail}\n{options}\
             Decide: allow, deny, answer (for a question: give the answer), or escalate to the developer \
             when it's ambiguous, risky, or not clearly needed for the feature. Be conservative.",
            brief = brief(cx.feature),
            kind = d.kind,
            risk = d.risk,
            summary = d.summary,
            detail = d.detail.clone().unwrap_or_default(),
            options = if d.options.is_empty() {
                String::new()
            } else {
                format!("Options: {}\n", d.options.join(" | "))
            },
        );
        let schema = json!({
            "type": "object",
            "properties": {
                "choice": {"enum": ["allow", "deny", "answer", "escalate"]},
                "answer": {"type": "string"},
                "rationale": {"type": "string"}
            },
            "required": ["choice", "rationale"]
        });
        let v = self.ask(cx, &prompt, &schema, None).await?;
        let rationale = v["rationale"].as_str().unwrap_or_default().to_owned();
        Ok(match v["choice"].as_str() {
            Some("allow") => Verdict::Allow { rationale },
            Some("deny") => Verdict::Deny { rationale },
            Some("answer") => Verdict::Answer {
                text: v["answer"].as_str().unwrap_or_default().to_owned(),
                rationale,
            },
            _ => Verdict::Escalate { why: rationale },
        })
    }

    async fn judge(&self, cx: &Context<'_>) -> Result<Judgement> {
        let f = cx.feature;
        let criteria: Vec<_> = f
            .acceptance
            .iter()
            .map(|c| json!({"id": c.id, "text": c.text}))
            .collect();
        let evidence: Vec<_> = f
            .evidence
            .iter()
            .rev()
            .take(12)
            .map(|e| json!({"kind": e.kind, "title": e.title, "ok": e.ok, "detail": e.detail, "uncertain": e.uncertain}))
            .collect();
        let summaries: Vec<_> = f
            .runs
            .iter()
            .rev()
            .take(3)
            .filter_map(|r| r.summary.clone())
            .collect();
        let prompt = format!(
            "You are the Control Agent verifying a feature. Judge each acceptance criterion strictly from \
             the evidence (a deterministic check outranks a claim; the coding agent's own summary is a claim, \
             not evidence). If unsure, it is not met.\n{brief}\nCriteria: {criteria}\nEvidence (newest first): \
             {evidence}\nAgent summaries: {summaries:?}\nremediation: what to fix next if anything is not met.",
            brief = brief(f),
            criteria = serde_json::Value::Array(criteria),
            evidence = serde_json::Value::Array(evidence),
        );
        let schema = json!({
            "type": "object",
            "properties": {
                "criteria": {"type": "array", "items": {"type": "object", "properties": {
                    "id": {"type": "string"}, "met": {"type": "boolean"}, "reason": {"type": "string"}},
                    "required": ["id", "met", "reason"]}},
                "remediation": {"type": "string"}
            },
            "required": ["criteria", "remediation"]
        });
        let v = self.ask(cx, &prompt, &schema, None).await?;
        let mut j: Judgement = serde_json::from_value(v)?;
        // Only deterministic checks can overrule; a visual review can't.
        // A failed deterministic check can't be judged away.
        let last_check = f
            .evidence
            .iter()
            .rev()
            .find(|e| e.ok.is_some() && !e.uncertain);
        if last_check.is_some_and(|e| e.ok == Some(false)) {
            for c in &mut j.criteria {
                c.met = false;
            }
        }
        Ok(j)
    }

    async fn review(
        &self,
        cx: &Context<'_>,
        check: &str,
        screenshot: &Path,
    ) -> Result<Option<(bool, String)>> {
        let prompt = format!(
            "Look at the screenshot (from the browser check “{check}”) and judge whether the UI \
             looks right for this feature: nothing broken, overlapping or obviously wrong.\n{brief}\n\
             looks_right: your judgment; notes: one or two sentences.",
            brief = brief(cx.feature),
        );
        let schema = json!({
            "type": "object",
            "properties": {"looks_right": {"type": "boolean"}, "notes": {"type": "string"}},
            "required": ["looks_right", "notes"]
        });
        let v = self.ask(cx, &prompt, &schema, Some(screenshot)).await?;
        Ok(Some((
            v["looks_right"].as_bool().unwrap_or(false),
            v["notes"].as_str().unwrap_or_default().to_owned(),
        )))
    }

    async fn reply(&self, cx: &Context<'_>, message: &str, say: &Say<'_>) -> Result<Reply> {
        let f = cx.feature;
        let recent: Vec<String> = f
            .messages
            .iter()
            .rev()
            .take(12)
            .rev()
            .map(|m| format!("{:?}: {}", m.role, super::agents::excerpt(&m.text, 600)))
            .collect();
        let tasks: Vec<String> = f
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}. {} — {:?}", i + 1, t.title, t.status))
            .collect();
        let prompt = format!(
            "You are the Control Agent running a software feature for the developer. Reply to their \
             latest message directly and briefly (2-5 sentences), in the language they wrote in. \
             Say what is happening and what happens next; if they give instructions, say how you'll \
             act on them. Don't invent progress.\n{brief}\nStatus: {status}\nTasks:\n{tasks}\n\
             Recent conversation:\n{recent}\n\nTheir message: {message}\n\n\
             Write your reply as plain text. Then, on a last line of its own, write `INTENT: <x>` where \
             <x> is: revise if they change, add to or correct what should be built (new requirements, \
             a different approach, feedback on the result); continue if they just ask to go on, resume, \
             start or retry; pause if they ask to stop or wait for now; finish if they're satisfied and \
             want it wrapped up; else none (questions, chat).",
            brief = brief(f),
            status = status_text(f),
            tasks = tasks.join("\n"),
            recent = recent.join("\n"),
        );
        // The developer sees the reply as it's written, without the intent line.
        let written = std::sync::Mutex::new(String::new());
        let shown = std::sync::Mutex::new(0usize);
        let on_piece = |piece: &str| {
            let mut text = written.lock().unwrap();
            text.push_str(piece);
            let visible = visible_reply(&text);
            let mut shown = shown.lock().unwrap();
            if visible.len() > *shown {
                say(&visible[*shown..]);
                *shown = visible.len();
            }
        };
        let full = match self.backend {
            Backend::ClaudeCode => {
                crate::agents::claude_stream::stream_text(
                    cx.env,
                    cx.root,
                    &prompt,
                    self.model.as_deref(),
                    &on_piece,
                )
                .await?
            }
            Backend::Api(p) => {
                p.stream_text(
                    cx.env,
                    self.key(p)?,
                    self.model.as_deref(),
                    &prompt,
                    &on_piece,
                )
                .await?
            }
        };
        let (text, intent) = split_intent(&full);
        Ok(Reply { text, intent })
    }
}

/// A reply without its `INTENT:` line — also while it is still being written
/// (a last line that may be turning into one is held back).
pub fn visible_reply(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            let t = l.trim().to_uppercase();
            let partial_last = *i == n - 1 && !t.is_empty() && "INTENT:".starts_with(&t);
            !t.starts_with("INTENT:") && !partial_last
        })
        .map(|(_, l)| *l)
        .collect();
    let joined = kept.join("\n");
    // Don't show a trailing blank line that may precede the intent.
    joined.trim_end().to_owned()
}

/// The reply and the intent its last `INTENT:` line names (none if absent).
pub fn split_intent(text: &str) -> (String, Intent) {
    let intent = text
        .lines()
        .rev()
        .find_map(|l| {
            let t = l.trim();
            t.to_uppercase().starts_with("INTENT:").then(|| {
                t["INTENT:".len()..]
                    .trim()
                    .trim_matches(['`', '.', '*'])
                    .to_lowercase()
            })
        })
        .map(|w| match w.as_str() {
            "continue" => Intent::Continue,
            "pause" => Intent::Pause,
            "revise" => Intent::Revise,
            "finish" => Intent::Finish,
            _ => Intent::None,
        })
        .unwrap_or(Intent::None);
    (visible_reply(text).trim().to_owned(), intent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::feature::{Criterion, Evidence, EvidenceKind};

    #[test]
    fn the_intent_line_is_read_and_never_shown() {
        let (text, intent) = split_intent("On it: I'll add JSON export.\n\nINTENT: revise");
        assert_eq!(text, "On it: I'll add JSON export.");
        assert_eq!(intent, Intent::Revise);
        assert_eq!(
            split_intent("Sure.\nintent: `continue`").1,
            Intent::Continue
        );
        assert_eq!(split_intent("Just chatting.").1, Intent::None);
        // While it's being written, a line that may become the intent is held back.
        assert_eq!(visible_reply("Done soon.\nINT"), "Done soon.");
        assert_eq!(
            visible_reply("Done soon.\nIn the meantime"),
            "Done soon.\nIn the meantime"
        );
    }

    #[test]
    fn the_status_says_where_things_stand() {
        let mut f = feature();
        assert!(status_text(&f).contains("draft"));
        f.status = otter_core::feature::FeatureStatus::Paused;
        assert!(status_text(&f).contains("continue"));
    }

    fn feature() -> Feature {
        Feature::new(
            "CSV export".into(),
            "Export the timeline.\n- has a header row\n- escapes commas".into(),
            chrono::Utc::now(),
        )
    }

    #[test]
    fn rules_plan_reads_bullets_and_finds_the_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            "build:\n\ttrue\ntest:\n\ttrue\n",
        )
        .unwrap();
        let f = feature();
        let env = EnvMap::new();
        let plan = rules_plan(&Context {
            feature: &f,
            root: dir.path(),
            env: &env,
        });
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.verify_command.as_deref(), Some("make test"));
        assert_eq!(
            plan.criteria,
            vec!["has a header row", "escapes commas", "`make test` passes"]
        );
    }

    #[test]
    fn rules_judge_follows_the_latest_check() {
        let mut f = feature();
        f.acceptance = vec![Criterion {
            id: "ac_1".into(),
            text: "x".into(),
            met: None,
            evidence: vec![],
        }];
        let env = EnvMap::new();
        let root = Path::new("/w");
        let ev = |ok| Evidence {
            id: otter_core::EvidenceId::generate(),
            task_id: None,
            criterion_id: None,
            kind: EvidenceKind::Test,
            title: "make test".into(),
            ok: Some(ok),
            uri: None,
            detail: Some("1 failed".into()),
            uncertain: false,
            at: chrono::Utc::now(),
        };
        let judge = |f: &Feature| {
            rules_judge(&Context {
                feature: f,
                root,
                env: &env,
            })
        };
        assert!(!judge(&f).criteria[0].met);
        f.evidence.push(ev(false));
        let j = judge(&f);
        assert!(!j.criteria[0].met);
        assert_eq!(j.remediation, "1 failed");
        f.evidence.push(ev(true));
        assert!(judge(&f).criteria[0].met);
    }
}
