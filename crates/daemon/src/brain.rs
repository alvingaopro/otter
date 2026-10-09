//! The Control Agent's judgment (D-045): the few places a model decides —
//! writing the plan, deciding what policy leaves open, judging the evidence.
//! Everything else (lifecycle, limits, retries, what may run) is the
//! controller's deterministic code.
//!
//! `OTTER_CONTROLLER` picks the brain: `claude` (the default when Claude Code
//! is installed: `claude -p` with structured output, no tools), `openrouter`
//! (any model on OpenRouter, with `OPENROUTER_API_KEY` in otterd's
//! environment; the default when that is set and Claude Code isn't
//! installed), `rules` (no model: one task, every open decision goes to the
//! developer, criteria are met when the check passes), or `yes` (tests
//! only: approves everything it is asked — to show that policy still stops
//! it). `OTTER_CONTROLLER_MODEL` picks the model for either.

use std::path::Path;

use anyhow::{Result, bail};
use async_trait::async_trait;
use otter_core::feature::{DecisionKind, DecisionRequest, Feature};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::env::EnvMap;

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
}

/// What the brain is chosen from: the host's settings (D-048), with an
/// explicit environment winning.
#[derive(Clone, Debug, Default)]
pub struct Choice {
    /// `claude`, `openrouter`, `rules`, `yes`, `off`; `None`: automatic.
    pub controller: Option<String>,
    pub model: Option<String>,
    /// From settings, else otterd's environment.
    pub openrouter_key: Option<String>,
}

/// The brain chosen (automatic: Claude Code when installed, else OpenRouter
/// when a key is set, else rules).
pub fn select(env: &EnvMap, choice: &Choice) -> Box<dyn Brain> {
    let key = choice
        .openrouter_key
        .clone()
        .or_else(|| crate::openrouter::key(env));
    let model = |backend| {
        Box::new(Model {
            backend,
            model: choice.model.clone(),
            key: key.clone(),
        })
    };
    match choice.controller.as_deref().unwrap_or_default() {
        "rules" | "off" => Box::new(Rules),
        "yes" => Box::new(YesMan),
        "claude" => model(Backend::ClaudeCode),
        "openrouter" => model(Backend::OpenRouter),
        _ if crate::env::which("claude", env).is_some() => model(Backend::ClaudeCode),
        _ if key.is_some() => model(Backend::OpenRouter),
        _ => Box::new(Rules),
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Backend {
    /// `claude -p` with a JSON schema and no tools (the developer's Claude
    /// Code sign-in).
    ClaudeCode,
    /// OpenRouter's chat completions (`OPENROUTER_API_KEY`).
    OpenRouter,
}

/// A model deciding where judgment is needed.
pub struct Model {
    pub backend: Backend,
    /// The model to ask (`None`: the backend's default).
    pub model: Option<String>,
    /// OpenRouter's key.
    pub key: Option<String>,
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
            Backend::OpenRouter => {
                let key = self.key.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("no OpenRouter API key: set one in Settings on this host")
                })?;
                crate::openrouter::structured(
                    cx.env,
                    key,
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
            Backend::OpenRouter => "openrouter",
        }
    }

    async fn plan(&self, cx: &Context<'_>) -> Result<Plan> {
        let detected = detect_verify_command(cx.root);
        let prompt = format!(
            "You are the Control Agent planning a software feature for a coding agent working in {root}.\n\
             {brief}\n\
             Write: requirements (short), acceptance criteria that can be checked, and 1-5 tasks in order \
             (depends_on = indexes of earlier tasks). verify_command: one shell command that checks the work \
             (the project's tests{hint}), or null. rationale: one sentence.",
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::feature::{Criterion, Evidence, EvidenceKind};

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
