//! Delivery (D-047): publishing a verified feature as a pull request,
//! following its CI, and deciding when it may be called done.
//!
//! - **Forge:** GitHub through the host's own `gh` (its sign-in, its scope):
//!   Otter stores no token and passes none on a command line. Another forge
//!   is another implementation of the same few calls.
//! - **Publishing is an external action:** pushing the branch and opening the
//!   pull request needs the developer's approval once per feature (a
//!   `user_only` decision — `git push` is high risk to policy); later pushes
//!   of the same branch reuse it. Otter never merges or deploys.
//! - **Gates** are deterministic: requirements met, local tests, browser
//!   evidence where the project declares checks, the pull request, CI, and
//!   the developer's acceptance. Done needs every gate passed or not
//!   applicable (or the developer overriding, on record).
//! - **CI failing** sends the feature back to implementing with the failed
//!   log; a failure that looks like the infrastructure (a runner lost, a
//!   network error) is rerun instead, at most twice per pushed commit.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use otter_core::feature::{
    CiCheck, Decider, DecisionStatus, EvidenceKind, Feature, Gate, GateStatus,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::env::{EnvMap, which};

pub const ACCEPTANCE_GATE: &str = "Your acceptance";
/// Reruns of a failure that looks like infrastructure, per pushed commit.
pub const MAX_RERUNS: u32 = 2;
const LOG_TAIL: usize = 3000;

/// Failures that are the CI machinery's, not the change's.
const FLAKY: &[&str] = &[
    "runner has received a shutdown signal",
    "lost communication with the server",
    "the operation was canceled",
    "rate limit",
    "econnreset",
    "etimedout",
    "502 bad gateway",
    "503 service",
    "network is unreachable",
    "no space left on device",
    "could not resolve host",
];

/// The failure looks like the infrastructure's.
pub fn looks_flaky(text: &str) -> bool {
    let lower = text.to_lowercase();
    FLAKY.iter().any(|p| lower.contains(p))
}

/// What the gates need to know beyond the feature.
pub struct GateContext {
    /// A Git workspace with an `origin` and `gh` available.
    pub publishable: bool,
    /// The repository runs CI (`.github/workflows`).
    pub has_ci: bool,
    /// The project declares browser checks.
    pub browser_checks: bool,
    /// A push just happened: CI hasn't caught up with it yet.
    pub ci_settling: bool,
}

fn gate(name: &str, status: GateStatus, detail: impl Into<String>) -> Gate {
    let d: String = detail.into();
    Gate {
        name: name.into(),
        status,
        detail: Some(d).filter(|d| !d.is_empty()),
    }
}

/// Evaluate the gates (D-047).
pub fn gates(f: &Feature, cx: &GateContext) -> Vec<Gate> {
    let mut out = Vec::new();
    // Requirements: every acceptance criterion met.
    let unmet: Vec<&str> = f
        .acceptance
        .iter()
        .filter(|c| c.met != Some(true))
        .map(|c| c.text.as_str())
        .collect();
    out.push(if f.acceptance.is_empty() {
        gate(
            "Requirements met",
            GateStatus::Pending,
            "no acceptance criteria yet",
        )
    } else if unmet.is_empty() {
        gate(
            "Requirements met",
            GateStatus::Passed,
            format!("{} criteria", f.acceptance.len()),
        )
    } else {
        gate(
            "Requirements met",
            GateStatus::Failed,
            format!("not met: {}", unmet.join("; ")),
        )
    });
    // Local tests: the latest check.
    let test = f
        .evidence
        .iter()
        .rev()
        .find(|e| e.kind == EvidenceKind::Test);
    out.push(match (test, &f.verify_command) {
        (Some(e), _) if e.ok == Some(true) => {
            gate("Local tests", GateStatus::Passed, e.title.clone())
        }
        (Some(e), _) => gate("Local tests", GateStatus::Failed, e.title.clone()),
        (None, Some(cmd)) => gate(
            "Local tests",
            GateStatus::Pending,
            format!("`{cmd}` hasn't run"),
        ),
        (None, None) => gate("Local tests", GateStatus::NotApplicable, "no check command"),
    });
    // Browser evidence, where the project declares checks.
    let browser = f
        .evidence
        .iter()
        .rev()
        .find(|e| e.kind == EvidenceKind::Browser);
    out.push(match (cx.browser_checks, browser) {
        (false, _) => gate(
            "Browser evidence",
            GateStatus::NotApplicable,
            "no browser checks declared",
        ),
        (true, Some(e)) if e.ok == Some(true) => {
            gate("Browser evidence", GateStatus::Passed, e.title.clone())
        }
        (true, Some(e)) => gate("Browser evidence", GateStatus::Failed, e.title.clone()),
        (true, None) => gate("Browser evidence", GateStatus::Pending, "not run yet"),
    });
    // The pull request.
    let d = f.delivery.as_ref();
    out.push(match d {
        _ if !cx.publishable => gate(
            "Pull request",
            GateStatus::NotApplicable,
            "not a Git workspace with a GitHub remote",
        ),
        Some(d) if d.declined => gate(
            "Pull request",
            GateStatus::NotApplicable,
            "you chose not to publish",
        ),
        Some(d) if d.pr_url.is_some() => gate(
            "Pull request",
            GateStatus::Passed,
            d.pr_url.clone().unwrap_or_default(),
        ),
        _ => gate("Pull request", GateStatus::Pending, "waiting to publish"),
    });
    // CI on the pull request.
    out.push(match d {
        Some(d) if d.pr_url.is_some() => {
            let failed: Vec<&str> =
                d.ci.iter()
                    .filter(|c| matches!(c.state.as_str(), "fail" | "cancel"))
                    .map(|c| c.name.as_str())
                    .collect();
            let pending = d.ci.iter().any(|c| c.state == "pending");
            if cx.ci_settling {
                gate(
                    "CI",
                    GateStatus::Pending,
                    "waiting for CI on the new commit",
                )
            } else if !failed.is_empty() {
                gate(
                    "CI",
                    GateStatus::Failed,
                    format!("failed: {}", failed.join(", ")),
                )
            } else if pending || (d.ci.is_empty() && cx.has_ci) {
                gate("CI", GateStatus::Pending, "running")
            } else if d.ci.is_empty() {
                gate("CI", GateStatus::NotApplicable, "no CI checks")
            } else {
                gate(
                    "CI",
                    GateStatus::Passed,
                    format!("{} checks passed", d.ci.len()),
                )
            }
        }
        _ if !cx.publishable || d.is_some_and(|d| d.declined) => {
            gate("CI", GateStatus::NotApplicable, "nothing published")
        }
        _ => gate("CI", GateStatus::Pending, "after the pull request"),
    });
    let accepted = f
        .gates
        .iter()
        .any(|g| g.name == ACCEPTANCE_GATE && g.status == GateStatus::Passed);
    out.push(gate(
        ACCEPTANCE_GATE,
        if accepted {
            GateStatus::Passed
        } else {
            GateStatus::Pending
        },
        "",
    ));
    out
}

/// Every gate but the developer's acceptance is clear.
pub fn ready_for_acceptance(gates: &[Gate]) -> bool {
    gates
        .iter()
        .filter(|g| g.name != ACCEPTANCE_GATE)
        .all(|g| g.status.clear())
}

/// The final report: what changed, how it was checked, what was decided,
/// what is still open.
pub fn report(f: &Feature, diffstat: Option<&str>) -> String {
    let mut r = format!("# {}\n\n{}\n", f.title, f.request);
    if let Some(d) = &f.delivery
        && let Some(url) = &d.pr_url
    {
        r.push_str(&format!("\nPull request: {url} (branch `{}`)\n", d.branch));
    }
    if let Some(stat) = diffstat.filter(|s| !s.trim().is_empty()) {
        r.push_str(&format!(
            "\n## Changed files\n\n```\n{}\n```\n",
            stat.trim()
        ));
    }
    r.push_str("\n## Acceptance criteria\n\n");
    for c in &f.acceptance {
        let mark = match c.met {
            Some(true) => "✓",
            Some(false) => "✗",
            None => "?",
        };
        r.push_str(&format!("- {mark} {}\n", c.text));
    }
    r.push_str("\n## Gates\n\n");
    for g in &f.gates {
        r.push_str(&format!(
            "- {} — {:?}{}\n",
            g.name,
            g.status,
            g.detail
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ));
    }
    r.push_str("\n## Evidence\n\n");
    for e in &f.evidence {
        let mark = match e.ok {
            Some(true) => "✓",
            Some(false) => "✗",
            None => "·",
        };
        let note = if e.uncertain {
            " (a model's judgment)"
        } else {
            ""
        };
        r.push_str(&format!("- {mark} {}{note}\n", e.title));
    }
    let decided: Vec<_> = f
        .decisions
        .iter()
        .filter(|d| d.decided_by.is_some_and(|by| by != Decider::Policy))
        .collect();
    if !decided.is_empty() {
        r.push_str("\n## Decisions\n\n");
        for d in decided {
            r.push_str(&format!(
                "- {} — {:?} by {:?}{}\n",
                d.summary,
                d.status,
                d.decided_by.unwrap(),
                d.rationale
                    .as_deref()
                    .map(|x| format!(": {x}"))
                    .unwrap_or_default()
            ));
        }
    }
    let mut risks = Vec::new();
    for g in f
        .gates
        .iter()
        .filter(|g| g.name != ACCEPTANCE_GATE && !g.status.clear())
    {
        risks.push(format!("{} is {:?}", g.name, g.status).to_lowercase());
    }
    for e in f.evidence.iter().filter(|e| e.uncertain) {
        risks.push(format!("“{}” is a model's judgment, not a check", e.title));
    }
    for d in f
        .decisions
        .iter()
        .filter(|d| d.status == DecisionStatus::Denied)
    {
        risks.push(format!("denied: {}", d.summary));
    }
    for line in f
        .rationale
        .iter()
        .filter(|l| l.starts_with("Accepted by you despite"))
    {
        risks.push(line.clone());
    }
    r.push_str("\n## Unresolved risks\n\n");
    if risks.is_empty() {
        r.push_str("None known.\n");
    } else {
        for x in risks {
            r.push_str(&format!("- {x}\n"));
        }
    }
    r.push_str("\n_Written by Otter's Lead._\n");
    r
}

/// Run a program in `cwd` with `env`; stdout, or an error with stderr.
/// `input` goes to stdin (bodies never go on the command line).
async fn run(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    env: &EnvMap,
    input: Option<&str>,
) -> Result<(bool, String, String)> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    if let Some(text) = input {
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(text.as_bytes()).await?;
    }
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "{} {} timed out",
            program.display(),
            args.first().unwrap_or(&"")
        )
    })??;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn tail(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    chars[chars.len().saturating_sub(n)..].iter().collect()
}

/// Git in a workspace.
pub struct Git<'a> {
    pub root: &'a Path,
    pub env: &'a EnvMap,
}

impl Git<'_> {
    async fn git(&self, args: &[&str]) -> Result<String> {
        let git = which("git", self.env).ok_or_else(|| anyhow!("git is not installed"))?;
        let (ok, out, err) = run(&git, args, self.root, self.env, None).await?;
        if !ok {
            bail!("git {}: {}", args.join(" "), tail(err.trim(), 500));
        }
        Ok(out.trim().to_owned())
    }

    /// The branch checked out, if this is a repository with an `origin`.
    pub async fn publishable_branch(&self) -> Option<String> {
        self.git(&["remote", "get-url", "origin"]).await.ok()?;
        let b = self
            .git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .ok()?;
        (b != "HEAD").then_some(b)
    }

    pub async fn head(&self) -> Result<String> {
        self.git(&["rev-parse", "HEAD"]).await
    }

    /// What the branch changes against its base, by file.
    pub async fn diffstat(&self, base: Option<&str>) -> Option<String> {
        let base = base.unwrap_or("origin/HEAD");
        self.git(&["diff", "--stat", &format!("{base}...HEAD")])
            .await
            .ok()
    }

    pub async fn push(&self, branch: &str) -> Result<()> {
        self.git(&["push", "-u", "origin", &format!("HEAD:refs/heads/{branch}")])
            .await
            .map(|_| ())
    }
}

/// GitHub, through the host's `gh`.
pub struct GitHub<'a> {
    pub gh: std::path::PathBuf,
    pub root: &'a Path,
    pub env: &'a EnvMap,
}

impl<'a> GitHub<'a> {
    pub fn find(root: &'a Path, env: &'a EnvMap) -> Option<GitHub<'a>> {
        Some(GitHub {
            gh: which("gh", env)?,
            root,
            env,
        })
    }

    async fn gh(&self, args: &[&str], input: Option<&str>) -> Result<String> {
        let (ok, out, err) = run(&self.gh, args, self.root, self.env, input).await?;
        if !ok {
            bail!("gh {}: {}", args.join(" "), tail(err.trim(), 500));
        }
        Ok(out)
    }

    /// The open pull request for `branch`, if there is one.
    pub async fn find_pr(&self, branch: &str) -> Option<(u64, String)> {
        let out = self
            .gh(&["pr", "view", branch, "--json", "number,url,state"], None)
            .await
            .ok()?;
        let v: Value = serde_json::from_str(&out).ok()?;
        (v["state"] == "OPEN").then(|| {
            (
                v["number"].as_u64().unwrap_or(0),
                v["url"].as_str().unwrap_or_default().to_owned(),
            )
        })
    }

    /// Open a pull request (or update the open one's description).
    pub async fn publish(
        &self,
        branch: &str,
        base: Option<&str>,
        title: &str,
        body: &str,
    ) -> Result<(u64, String)> {
        if let Some((n, url)) = self.find_pr(branch).await {
            self.gh(
                &["pr", "edit", &n.to_string(), "--body-file", "-"],
                Some(body),
            )
            .await?;
            return Ok((n, url));
        }
        let mut args = vec![
            "pr",
            "create",
            "--head",
            branch,
            "--title",
            title,
            "--body-file",
            "-",
        ];
        if let Some(b) = base {
            args.extend(["--base", b]);
        }
        let out = self.gh(&args, Some(body)).await?;
        let url = out
            .lines()
            .rev()
            .find(|l| l.starts_with("http"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let n = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if url.is_empty() {
            bail!("gh pr create gave no URL");
        }
        Ok((n, url))
    }

    /// The pull request's checks (`pending` while CI hasn't reported).
    pub async fn checks(&self, pr: u64) -> Result<Vec<CiCheck>> {
        // `gh pr checks` exits non-zero while checks fail or wait: read
        // its output either way.
        let (_, out, err) = run(
            &self.gh,
            &[
                "pr",
                "checks",
                &pr.to_string(),
                "--json",
                "name,bucket,link,description",
            ],
            self.root,
            self.env,
            None,
        )
        .await?;
        if out.trim().is_empty() {
            if err.contains("no checks") {
                return Ok(Vec::new());
            }
            bail!("gh pr checks: {}", tail(err.trim(), 300));
        }
        let v: Vec<Value> = serde_json::from_str(&out).context("reading gh pr checks")?;
        Ok(v.iter()
            .map(|c| CiCheck {
                name: c["name"].as_str().unwrap_or("?").to_owned(),
                state: c["bucket"].as_str().unwrap_or("pending").to_owned(),
                url: c["link"]
                    .as_str()
                    .map(String::from)
                    .filter(|s| !s.is_empty()),
                description: c["description"]
                    .as_str()
                    .map(String::from)
                    .filter(|s| !s.is_empty()),
            })
            .collect())
    }

    /// The Actions run a check belongs to (`…/actions/runs/<id>/…`).
    fn run_id(url: &str) -> Option<&str> {
        url.split("/actions/runs/").nth(1)?.split('/').next()
    }

    /// The failed jobs' log, tail end.
    pub async fn failed_log(&self, check: &CiCheck) -> Option<String> {
        let id = Self::run_id(check.url.as_deref()?)?;
        let out = self
            .gh(&["run", "view", id, "--log-failed"], None)
            .await
            .ok()?;
        Some(tail(&out, LOG_TAIL))
    }

    pub async fn rerun(&self, check: &CiCheck) -> Result<()> {
        let id = check
            .url
            .as_deref()
            .and_then(Self::run_id)
            .ok_or_else(|| anyhow!("no run to rerun for {}", check.name))?;
        self.gh(&["run", "rerun", id, "--failed"], None)
            .await
            .map(|_| ())
    }

    pub async fn comment(&self, pr: u64, body: &str) -> Result<()> {
        self.gh(
            &["pr", "comment", &pr.to_string(), "--body-file", "-"],
            Some(body),
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::feature::{Criterion, Delivery, Evidence};

    fn feature() -> Feature {
        let mut f = Feature::new("CSV".into(), "Export".into(), chrono::Utc::now());
        f.acceptance.push(Criterion {
            id: "ac_1".into(),
            text: "exports".into(),
            met: Some(true),
            evidence: vec![],
        });
        f.verify_command = Some("make test".into());
        f.evidence.push(Evidence {
            id: otter_core::EvidenceId::generate(),
            task_id: None,
            criterion_id: None,
            kind: EvidenceKind::Test,
            title: "`make test` (exit 0)".into(),
            ok: Some(true),
            uri: None,
            detail: None,
            uncertain: false,
            at: chrono::Utc::now(),
        });
        f
    }

    fn status(g: &[Gate], name: &str) -> GateStatus {
        g.iter().find(|x| x.name == name).unwrap().status
    }

    fn delivery(ci: &[(&str, &str)]) -> Delivery {
        Delivery {
            branch: "feat".into(),
            base: None,
            head: Some("abc".into()),
            pushed_at: None,
            pr_url: Some("https://github.com/o/r/pull/7".into()),
            pr_number: Some(7),
            ci: ci
                .iter()
                .map(|(n, s)| CiCheck {
                    name: (*n).into(),
                    state: (*s).into(),
                    url: None,
                    description: None,
                })
                .collect(),
            reruns: 0,
            declined: false,
        }
    }

    #[test]
    fn a_local_feature_needs_only_its_checks_and_you() {
        let f = feature();
        let g = gates(
            &f,
            &GateContext {
                publishable: false,
                has_ci: false,
                browser_checks: false,
                ci_settling: false,
            },
        );
        assert_eq!(status(&g, "Requirements met"), GateStatus::Passed);
        assert_eq!(status(&g, "Local tests"), GateStatus::Passed);
        assert_eq!(status(&g, "Pull request"), GateStatus::NotApplicable);
        assert_eq!(status(&g, "CI"), GateStatus::NotApplicable);
        assert_eq!(status(&g, ACCEPTANCE_GATE), GateStatus::Pending);
        assert!(ready_for_acceptance(&g));
    }

    #[test]
    fn a_published_feature_waits_for_green_ci() {
        let mut f = feature();
        let cx = GateContext {
            publishable: true,
            has_ci: true,
            browser_checks: true,
            ci_settling: false,
        };
        let g = gates(&f, &cx);
        assert_eq!(status(&g, "Pull request"), GateStatus::Pending);
        assert_eq!(status(&g, "Browser evidence"), GateStatus::Pending);
        assert!(!ready_for_acceptance(&g));

        f.delivery = Some(delivery(&[]));
        assert_eq!(
            status(&gates(&f, &cx), "CI"),
            GateStatus::Pending,
            "CI hasn't reported yet"
        );
        f.delivery = Some(delivery(&[("test", "pass"), ("lint", "pending")]));
        assert_eq!(status(&gates(&f, &cx), "CI"), GateStatus::Pending);
        f.delivery = Some(delivery(&[("test", "fail"), ("lint", "pass")]));
        let g = gates(&f, &cx);
        assert_eq!(status(&g, "CI"), GateStatus::Failed);
        assert!(
            g.iter()
                .any(|x| x.detail.as_deref() == Some("failed: test"))
        );
        f.delivery = Some(delivery(&[("test", "pass"), ("lint", "skipping")]));
        assert_eq!(status(&gates(&f, &cx), "CI"), GateStatus::Passed);
        // Right after a push, what CI says is about the previous commit.
        let settling = GateContext {
            ci_settling: true,
            ..cx
        };
        f.delivery = Some(delivery(&[("test", "fail")]));
        assert_eq!(status(&gates(&f, &settling), "CI"), GateStatus::Pending);
        let cx = GateContext {
            ci_settling: false,
            ..settling
        };
        // Declining to publish takes the PR and CI gates out of the way.
        f.delivery = Some(Delivery {
            declined: true,
            pr_url: None,
            ..delivery(&[])
        });
        let g = gates(&f, &cx);
        assert_eq!(status(&g, "Pull request"), GateStatus::NotApplicable);
        assert_eq!(status(&g, "CI"), GateStatus::NotApplicable);
    }

    #[test]
    fn infrastructure_failures_are_told_apart() {
        assert!(looks_flaky(
            "Error: The runner has received a shutdown signal."
        ));
        assert!(looks_flaky("npm ERR! code ECONNRESET"));
        assert!(!looks_flaky("assertion failed: left == right"));
        assert!(!looks_flaky("error[E0425]: cannot find value"));
    }

    #[test]
    fn the_report_lists_what_is_still_open() {
        let mut f = feature();
        f.delivery = Some(delivery(&[("test", "pass")]));
        f.gates = gates(
            &f,
            &GateContext {
                publishable: true,
                has_ci: true,
                browser_checks: false,
                ci_settling: false,
            },
        );
        f.rationale
            .push("Accepted by you despite: ci (pending)".into());
        let r = report(&f, Some(" src/csv.rs | 40 +++++\n 1 file changed"));
        for want in [
            "# CSV",
            "Pull request: https://github.com/o/r/pull/7",
            "src/csv.rs",
            "✓ exports",
            "## Unresolved risks",
            "Accepted by you despite",
        ] {
            assert!(r.contains(want), "{want} in\n{r}");
        }
    }

    #[test]
    fn run_ids_come_from_check_links() {
        assert_eq!(
            GitHub::run_id("https://github.com/o/r/actions/runs/123/job/456"),
            Some("123")
        );
        assert_eq!(GitHub::run_id("https://ci.example/x"), None);
    }
}
