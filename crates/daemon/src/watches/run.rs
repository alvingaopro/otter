//! `run`: a GitHub Actions run, by its link, until it finishes (D-193) —
//! one the task didn't start: a run the developer pasted, a deploy, a
//! nightly. Read with `gh --repo` from Otter's own directory.

use std::time::Duration;

use async_trait::async_trait;
use otter_core::task::Task;
use otter_core::watch::{Finding, Outcome, Watch};
use serde_json::{Value, json};

use super::{Probe, ProbeContext, Target, WatchKind, env_ms, minutes_arg};
use crate::delivery::{GitHub, GithubLink};

pub(crate) struct Run;

/// Its deadline unless the model says otherwise: two hours.
const DEFAULT_MINUTES: u32 = 120;

#[async_trait]
impl WatchKind for Run {
    fn name(&self) -> &'static str {
        "run"
    }

    fn target(&self, _f: &Task, args: &Value, _what: &str) -> Result<Target, String> {
        let url = args["url"].as_str().unwrap_or_default();
        let Some(GithubLink::Run { repo, id, .. }) = GithubLink::parse(url) else {
            return Err("a run watch needs the url of a GitHub Actions run".into());
        };
        Ok(Target {
            value: json!({ "repo": repo, "run": id }),
            label: format!("run {id} in {repo}"),
            key: format!("run{id}"),
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
        let (Some(repo), Some(id)) = (w.target["repo"].as_str(), w.target["run"].as_str()) else {
            return Probe::Unreadable("the watch doesn't name a run".into());
        };
        let Some(mut gh) = GitHub::find(cx.dir, cx.env) else {
            return Probe::Unreadable("gh isn't installed on this host".into());
        };
        gh.repo = Some(repo.to_owned());
        let run = match gh.run_view(id).await {
            Ok(r) => r,
            Err(e) => return Probe::Unreadable(otter_core::redact::redact(&format!("{e:#}"))),
        };
        found(&run)
    }

    fn line(&self, w: &Watch, finding: &Finding) -> String {
        format!(
            "Run {}: {}.",
            w.target["run"].as_str().unwrap_or("?"),
            finding.text
        )
    }
}

/// What a run's state comes to: still going, or how it ended.
fn found(run: &crate::delivery::RunView) -> Probe {
    let name = if run.name.is_empty() {
        "the run".to_owned()
    } else {
        run.name.clone()
    };
    if run.status != "completed" {
        let running = run.jobs.iter().filter(|j| j.status != "completed").count();
        let status = run.status.replace('_', " ");
        return Probe::NotYet {
            key: format!("{}:{running}", run.status),
            text: if run.jobs.is_empty() {
                format!("{name}: {status}")
            } else {
                format!(
                    "{name}: {status}, {running} of {} jobs not finished",
                    run.jobs.len()
                )
            },
        };
    }
    let failed: Vec<&str> = run
        .jobs
        .iter()
        .filter(|j| matches!(j.conclusion.as_str(), "failure" | "timed_out" | "cancelled"))
        .map(|j| j.name.as_str())
        .collect();
    let at = if run.commit.is_empty() {
        String::new()
    } else {
        format!(" (at {})", &run.commit[..run.commit.len().min(7)])
    };
    match run.conclusion.as_str() {
        "success" | "skipped" | "neutral" => Probe::Found {
            outcome: Outcome::Passed,
            text: format!("{name} {}{at}", run.conclusion),
        },
        "cancelled" => Probe::Found {
            outcome: Outcome::Gone,
            text: format!("{name} was cancelled{at}"),
        },
        other => Probe::Found {
            outcome: Outcome::Failed,
            text: if failed.is_empty() {
                format!("{name}: {other}{at}")
            } else {
                format!("{name}: {other}{at}; failed: {}", failed.join(", "))
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{JobView, RunView};

    fn run(status: &str, conclusion: &str, jobs: &[(&str, &str)]) -> RunView {
        RunView {
            name: "CI".into(),
            status: status.into(),
            conclusion: conclusion.into(),
            commit: "0123456789".into(),
            jobs: jobs
                .iter()
                .map(|(s, c)| JobView {
                    name: format!("job-{s}-{c}"),
                    status: (*s).into(),
                    conclusion: (*c).into(),
                    failed_steps: vec![],
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_run_is_watched_until_it_finishes_and_says_how() {
        match found(&run(
            "in_progress",
            "",
            &[("completed", "success"), ("in_progress", "")],
        )) {
            Probe::NotYet { text, .. } => {
                assert_eq!(text, "CI: in progress, 1 of 2 jobs not finished")
            }
            _ => panic!("still going"),
        }
        match found(&run(
            "completed",
            "failure",
            &[("completed", "success"), ("completed", "failure")],
        )) {
            Probe::Found { outcome, text } => {
                assert_eq!(outcome, Outcome::Failed);
                assert_eq!(
                    text,
                    "CI: failure (at 0123456); failed: job-completed-failure"
                );
            }
            _ => panic!("it ended"),
        }
        assert!(matches!(
            found(&run("completed", "success", &[])),
            Probe::Found {
                outcome: Outcome::Passed,
                ..
            }
        ));
        assert!(matches!(
            found(&run("completed", "cancelled", &[])),
            Probe::Found {
                outcome: Outcome::Gone,
                ..
            }
        ));
    }

    #[test]
    fn only_a_run_link_is_watched_as_a_run() {
        let f = otter_core::task::Task::new("t".into(), "r".into(), chrono::Utc::now());
        let t = Run
            .target(
                &f,
                &json!({"url": "https://github.com/o/r/actions/runs/42"}),
                "x",
            )
            .unwrap();
        assert_eq!(t.key, "run42");
        assert_eq!(t.value, json!({"repo": "o/r", "run": "42"}));
        assert!(
            Run.target(&f, &json!({"url": "https://github.com/o/r/pull/7"}), "x")
                .is_err()
        );
        assert!(Run.target(&f, &json!({}), "x").is_err());
    }
}
