//! `ci`: the task's pull request's CI, until every check has finished —
//! or it can't run, or the pull request is merged or closed (D-183, D-184).
//! Read with `gh --repo`, from Otter's own directory: the workspace may be
//! put away by then.

use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use otter_core::task::Task;
use otter_core::watch::{Finding, Outcome, Watch, repo_of};
use serde_json::{Value, json};

use super::{Probe, ProbeContext, Target, WatchKind, env_ms, minutes_arg};
use crate::delivery::{CONFLICTING, GitHub};

pub(crate) struct Ci;

/// Its deadline unless the model says otherwise: two hours.
const DEFAULT_MINUTES: u32 = 120;

#[async_trait]
impl WatchKind for Ci {
    fn name(&self) -> &'static str {
        "ci"
    }

    fn target(&self, f: &Task, _args: &Value, _what: &str) -> Result<Target, String> {
        let d = f.delivery.as_ref();
        let (Some(pr), Some(url)) = (
            d.and_then(|d| d.pr_number),
            d.and_then(|d| d.pr_url.as_deref()),
        ) else {
            return Err(
                "this task has no pull request yet: there's no CI of its own to watch".into(),
            );
        };
        if f.gates
            .iter()
            .any(|g| g.name == "CI" && g.status == otter_core::task::GateStatus::NotApplicable)
        {
            return Err(
                "its repository runs no CI on the pull request: there's nothing to watch".into(),
            );
        }
        let repo = repo_of(url).ok_or_else(|| format!("`{url}` isn't a GitHub pull request"))?;
        Ok(Target {
            value: json!({ "pr": pr, "repo": repo }),
            label: format!("CI on #{pr}"),
            key: format!("pr{pr}"),
        })
    }

    fn minutes(&self, args: &Value) -> Result<u32, String> {
        Ok(minutes_arg(args, "for_minutes")?.unwrap_or(DEFAULT_MINUTES))
    }

    fn every(&self) -> Option<Duration> {
        Some(env_ms("OTTER_WATCH_POLL_MS", 60_000))
    }

    fn at_deadline(&self, w: &Watch) -> (Outcome, String) {
        let last = w
            .last_reading()
            .map(|r| format!("last read: {}", r.text))
            .unwrap_or_else(|| "nothing could be read".into());
        (
            Outcome::Expired,
            format!("not finished by its deadline; {last}"),
        )
    }

    async fn probe(&self, w: &Watch, cx: &ProbeContext<'_>) -> Probe {
        let (Some(pr), Some(repo)) = (w.target["pr"].as_u64(), w.target["repo"].as_str()) else {
            return Probe::Unreadable("the watch doesn't name a pull request".into());
        };
        let Some(mut gh) = GitHub::find(cx.dir, cx.env) else {
            return Probe::Unreadable("gh isn't installed on this host".into());
        };
        gh.repo = Some(repo.to_owned());
        let state = match gh.pr_state(pr).await {
            Ok(s) => s,
            Err(e) => return Probe::Unreadable(otter_core::redact::redact(&format!("{e:#}"))),
        };
        match state.state.as_str() {
            "MERGED" => {
                return Probe::Found {
                    outcome: Outcome::Gone,
                    text: "the pull request was merged".into(),
                };
            }
            "CLOSED" => {
                return Probe::Found {
                    outcome: Outcome::Gone,
                    text: "the pull request was closed without merging".into(),
                };
            }
            _ => {}
        }
        let checks = match gh.checks(pr).await {
            Ok(c) => c,
            Err(e) => return Probe::Unreadable(otter_core::redact::redact(&format!("{e:#}"))),
        };
        let head: String = state.head.chars().take(7).collect();
        let base = if state.base.is_empty() {
            "its base".to_owned()
        } else {
            state.base.clone()
        };
        // GitHub runs no `pull_request` workflow on a pull request that
        // conflicts (D-184): waiting for its CI is waiting for nothing.
        let conflicts = state.mergeable == CONFLICTING;
        if conflicts && checks.is_empty() {
            return Probe::Found {
                outcome: Outcome::Stuck,
                text: format!(
                    "it conflicts with {base}, so GitHub runs no CI on it until the branch is brought up to date"
                ),
            };
        }
        if checks.is_empty() {
            // Measured from when this head was first seen with no checks: a
            // push by anyone starts it again.
            let none = format!("{head}:none");
            let Some(since) = w.readings.iter().find(|r| r.key == none).map(|r| r.at) else {
                return Probe::NotYet {
                    key: none,
                    text: "no checks yet".into(),
                };
            };
            let start = chrono::Duration::from_std(env_ms("OTTER_CI_START_MS", 600_000))
                .unwrap_or_default();
            let gone = Utc::now() - since;
            if gone > start {
                return Probe::Found {
                    outcome: Outcome::Stuck,
                    text: format!(
                        "no CI check has started on it in {} min",
                        gone.num_minutes()
                    ),
                };
            }
            return Probe::NotYet {
                key: none,
                text: "no checks yet".into(),
            };
        }
        let count = |states: &[&str]| {
            checks
                .iter()
                .filter(|c| states.contains(&c.state.as_str()))
                .count()
        };
        let (passed, failed, pending) = (
            count(&["pass", "skipping"]),
            count(&["fail", "cancel"]),
            count(&["pending"]),
        );
        let n = checks.len();
        let failed_names = || {
            checks
                .iter()
                .filter(|c| matches!(c.state.as_str(), "fail" | "cancel"))
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let conflict = if conflicts {
            format!("; it conflicts with {base}")
        } else {
            String::new()
        };
        if pending > 0 {
            let mut text = format!("{pending} of {n} checks running, {passed} passed");
            if failed > 0 {
                text.push_str(&format!(", failed: {}", failed_names()));
            }
            return Probe::NotYet {
                key: format!("{head}:{passed}/{failed}/{pending}:{conflicts}"),
                text: format!("{text} (at {head}){conflict}"),
            };
        }
        // Every check finished, but it can't be merged as it is.
        if conflicts {
            let checks = if failed > 0 {
                format!("failed: {}", failed_names())
            } else {
                format!("all {n} checks passed")
            };
            return Probe::Found {
                outcome: Outcome::Stuck,
                text: format!(
                    "{checks} (at {head}), but it conflicts with {base}: it can't be merged as it is"
                ),
            };
        }
        if failed > 0 {
            return Probe::Found {
                outcome: Outcome::Failed,
                text: format!(
                    "failed: {} ({failed} of {n} checks, at {head})",
                    failed_names()
                ),
            };
        }
        Probe::Found {
            outcome: Outcome::Passed,
            text: format!("all {n} checks passed (at {head})"),
        }
    }

    fn line(&self, w: &Watch, finding: &Finding) -> String {
        let mut label = w.label.clone();
        if let Some(c) = label.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        format!("{label}: {}.", finding.text)
    }
}
