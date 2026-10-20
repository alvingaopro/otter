//! Delivery (D-047): publishing a verified task as a pull request,
//! following its CI, and deciding when it may be called done.
//!
//! - **Forge:** GitHub through the host's own `gh` (its sign-in, its scope):
//!   Otter stores no token and passes none on a command line. Another forge
//!   is another implementation of the same few calls.
//! - **Publishing is an external action:** pushing the branch and opening the
//!   pull request needs the developer's approval once per task (a
//!   `user_only` decision — `git push` is high risk to policy); later pushes
//!   of the same branch reuse it. Otter never merges or deploys.
//! - **Gates** are deterministic: requirements met, local tests, browser
//!   evidence where the project declares checks, the pull request, CI, and
//!   the developer's acceptance. Done needs every gate passed or not
//!   applicable (or the developer overriding, on record).
//! - **CI failing** sends the task back to implementing with the failed
//!   log; a failure that looks like the infrastructure (a runner lost, a
//!   network error) is rerun instead, at most twice per pushed commit.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use otter_core::task::{CiCheck, Decider, DecisionStatus, EvidenceKind, Gate, GateStatus, Task};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::env::{EnvMap, which};

pub const ACCEPTANCE_GATE: &str = "Your acceptance";
/// Reruns of a failure that looks like infrastructure, per pushed commit.
pub const MAX_RERUNS: u32 = 2;
/// A failed CI log keeps its last lines, each cut at so many characters.
const LOG_LINES: usize = 40;
const LOG_LINE: usize = 240;

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

/// A failed run's log (`gh run view --log-failed`) as the agent and the
/// developer read it (D-139). gh prints each line as `job⇥step⇥time text`,
/// with a BOM and a terminal's colours: here the job and step head their
/// lines once, without times or colours. It keeps the last [`LOG_LINES`]
/// lines, so the same failure keeps the same lines from one run to the
/// next, and takes credentials out before a line is cut, so no part of one
/// is left.
pub fn ci_log(raw: &str) -> String {
    let all: Vec<&str> = raw.lines().collect();
    let mut out = String::new();
    let mut section = None;
    for l in &all[all.len().saturating_sub(LOG_LINES)..] {
        let (head, text) = match l.splitn(3, '\t').collect::<Vec<_>>()[..] {
            [job, step, text] => (Some((job, step)), text),
            _ => (None, *l),
        };
        if let Some((job, step)) = head
            && head != section
        {
            out.push_str(&format!("[{job} › {step}]\n"));
            section = head;
        }
        out.push_str(&ci_line(text));
        out.push('\n');
    }
    otter_core::redact::redact(&out)
        .lines()
        .map(|l| l.chars().take(LOG_LINE).chain(['\n']).collect::<String>())
        .collect()
}

/// One line of a CI log without its timestamp, colours or BOM.
fn ci_line(text: &str) -> String {
    let c: Vec<char> = text.chars().filter(|&c| c != '\u{feff}').collect();
    let n = stamp(&c, 0);
    let start = if n > 0 && c.get(n).is_none_or(|&c| c == ' ') {
        (n + 1).min(c.len())
    } else {
        0
    };
    let mut out = String::new();
    let mut chars = c[start..].iter();
    while let Some(&c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
        } else if chars.clone().next() == Some(&'[') {
            // A colour: `ESC [`, its parameters, a final byte.
            chars.by_ref().skip(1).find(|c| ('@'..='~').contains(*c));
        }
    }
    out
}

/// Two failed CI logs show the same failure (D-139): the same lines once
/// what differs between runs of one failure is set aside — dates and
/// times, durations, where in a file (a fix moves lines), long numbers
/// (runs, processes, ports) and hashes.
pub fn same_ci_failure(a: &str, b: &str) -> bool {
    failure_key(a) == failure_key(b)
}

fn failure_key(log: &str) -> Vec<String> {
    log.lines()
        .map(|l| {
            let c: Vec<char> = l.chars().collect();
            let mut key = String::new();
            let mut i = 0;
            while i < c.len() {
                match varying(&c, i) {
                    0 => key.push(c[i]),
                    n => {
                        key.push('…');
                        i += n - 1;
                    }
                }
                i += 1;
            }
            key.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|l| !l.is_empty())
        .collect()
}

/// How many characters at `i` differ between runs of one failure, if any.
fn varying(c: &[char], i: usize) -> usize {
    let prev = i.checked_sub(1).map(|p| c[p]);
    if c[i].is_ascii_digit() && !prev.is_some_and(|p| p.is_ascii_digit()) {
        let n = [stamp(c, i), clock(c, i), position(c, i)]
            .into_iter()
            .max()
            .unwrap_or(0);
        if n > 0 {
            return n;
        }
    }
    if prev.is_some_and(|p| p.is_ascii_alphanumeric()) {
        return 0;
    }
    // A word: a long number or a hash, else maybe a duration.
    let word = &c[i..i + c[i..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count()];
    let digits = word.iter().filter(|c| c.is_ascii_digit()).count();
    if (digits == word.len() && digits >= 5)
        || (word.len() >= 7
            && digits > 0
            && digits < word.len()
            && word.iter().all(char::is_ascii_hexdigit))
    {
        return word.len();
    }
    if digits > 0 { duration(c, i) } else { 0 }
}

fn digits(c: &[char], i: usize) -> usize {
    c.get(i..)
        .map_or(0, |c| c.iter().take_while(|c| c.is_ascii_digit()).count())
}

/// A date, with its time if it has one: `2026-10-20T18:00:00.123Z`,
/// `2026-10-20 18:00:00,123`, `2026-10-20T18_00_00_123Z` (in a file name).
fn stamp(c: &[char], i: usize) -> usize {
    let d = |k: usize, n: usize| (0..n).all(|x| c.get(k + x).is_some_and(char::is_ascii_digit));
    let at = |k: usize, set: &str| c.get(k).is_some_and(|ch| set.contains(*ch));
    if !(d(i, 4) && at(i + 4, "-") && d(i + 5, 2) && at(i + 7, "-") && d(i + 8, 2)) {
        return 0;
    }
    let mut n = 10;
    if at(i + n, "T _") && d(i + n + 1, 2) && at(i + n + 3, ":_") && d(i + n + 4, 2) {
        n += 6;
        if at(i + n, ":_") && d(i + n + 1, 2) {
            n += 3;
        }
        if at(i + n, ".,_") && digits(c, i + n + 1) > 0 {
            n += 1 + digits(c, i + n + 1);
        }
        if at(i + n, "Z") {
            n += 1;
        } else if at(i + n, "+-") && d(i + n + 1, 2) {
            n += 3;
            if at(i + n, ":") && d(i + n + 1, 2) {
                n += 3;
            }
        }
    }
    n
}

/// A time of day: `18:00:00`, `0:01:05`, `18:00:00.123Z`.
fn clock(c: &[char], i: usize) -> usize {
    let h = digits(c, i);
    let colon = |k: usize| c.get(k) == Some(&':');
    if !(1..=2).contains(&h)
        || !colon(i + h)
        || digits(c, i + h + 1) != 2
        || !colon(i + h + 3)
        || digits(c, i + h + 4) != 2
    {
        return 0;
    }
    let mut n = h + 6;
    if c.get(i + n).is_some_and(|ch| ".,".contains(*ch)) && digits(c, i + n + 1) > 0 {
        n += 1 + digits(c, i + n + 1);
    }
    if c.get(i + n) == Some(&'Z') {
        n += 1;
    }
    n
}

/// Where in a file: `lib.rs:42:9`, `test_x.py:42:`, `line 42`.
fn position(c: &[char], i: usize) -> usize {
    let line = i >= 5
        && c[i - 5..i]
            .iter()
            .collect::<String>()
            .eq_ignore_ascii_case("line ");
    let file = i >= 3 && c[i - 1] == ':' && {
        let ext = c[..i - 1]
            .iter()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        let at = i - 1 - ext;
        (1..=5).contains(&ext) && c[at].is_ascii_alphabetic() && at > 0 && c[at - 1] == '.'
    };
    if !line && !file {
        return 0;
    }
    let mut n = digits(c, i);
    if c.get(i + n) == Some(&':') && digits(c, i + n + 1) > 0 {
        n += 1 + digits(c, i + n + 1);
    }
    n
}

/// A duration: `0.01s`, `12ms`, `1.5 sec`, `1m02.34s`.
fn duration(c: &[char], i: usize) -> usize {
    let mut end = 0;
    let mut j = i;
    loop {
        let mut k = j + digits(c, j);
        if k == j {
            break;
        }
        if c.get(k) == Some(&'.') && digits(c, k + 1) > 0 {
            k += 1 + digits(c, k + 1);
        }
        let gap = usize::from(c.get(k) == Some(&' '));
        let u = c[k + gap..]
            .iter()
            .take_while(|c| c.is_ascii_alphabetic())
            .count();
        if !is_unit(&c[k + gap..k + gap + u].iter().collect::<String>()) {
            break;
        }
        j = k + gap + u;
        end = j;
        // `1m02s` goes on; `1.5 sec` doesn't.
        if gap == 1 || !c.get(j).is_some_and(char::is_ascii_digit) {
            break;
        }
    }
    if end > 0 && !c.get(end).is_some_and(char::is_ascii_alphanumeric) {
        end - i
    } else {
        0
    }
}

fn is_unit(w: &str) -> bool {
    matches!(
        w,
        "ms" | "us"
            | "s"
            | "sec"
            | "secs"
            | "second"
            | "seconds"
            | "m"
            | "min"
            | "mins"
            | "minute"
            | "minutes"
            | "h"
            | "hr"
            | "hrs"
            | "hour"
            | "hours"
    )
}

/// What the gates need to know beyond the task.
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
pub fn gates(f: &Task, cx: &GateContext) -> Vec<Gate> {
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
pub fn report(f: &Task, diffstat: Option<&str>) -> String {
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
    r.push_str("\n_Written by Otter._\n");
    r
}

/// Run a program in `cwd` with `env`; stdout, or an error with stderr.
/// `input` goes to stdin (bodies never go on the command line).
pub(crate) async fn run(
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

/// The end of what a tool printed, without credentials (invariant 13).
fn tail(s: &str, n: usize) -> String {
    let s = otter_core::redact::redact(s);
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

    /// Changes a commit would take: anything changed or new but files that
    /// may hold secrets (by name, as `work_state` reads them).
    pub async fn committable(&self) -> bool {
        let mut args = vec![
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--",
            ".",
        ];
        let excluded = crate::work_state::secret_pathspecs();
        args.extend(excluded.iter().map(String::as_str));
        self.git(&args).await.is_ok_and(|s| !s.trim().is_empty())
    }

    /// Commit what the work left uncommitted, as `message` (D-068): the new
    /// HEAD, and the files left out because they may hold secrets. `None`:
    /// nothing to commit. Without an identity of the host's own, the commit
    /// is Otter's.
    pub async fn commit_work(&self, message: &str) -> Result<Option<(String, Vec<String>)>> {
        let status = self
            .git(&["status", "--porcelain=v1", "--untracked-files=all"])
            .await?;
        let left_out: Vec<String> = status
            .lines()
            .filter_map(|l| l.get(3..))
            .map(|p| {
                p.rsplit(" -> ")
                    .next()
                    .unwrap_or(p)
                    .trim_matches('"')
                    .to_owned()
            })
            .filter(|p| crate::work_state::secret_like(p))
            .collect();
        if !self.committable().await {
            return Ok(None);
        }
        let mut add = vec!["add", "-A", "--", "."];
        let excluded = crate::work_state::secret_pathspecs();
        add.extend(excluded.iter().map(String::as_str));
        self.git(&add).await?;
        let identity = self
            .git(&["config", "user.email"])
            .await
            .is_ok_and(|e| !e.is_empty());
        let mut commit = vec![];
        if !identity {
            commit.extend(["-c", "user.name=Otter", "-c", "user.email=otter@localhost"]);
        }
        commit.extend(["commit", "-q", "-m", message]);
        self.git(&commit).await?;
        Ok(Some((self.head().await?, left_out)))
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
                    .map(otter_core::redact::redact)
                    .filter(|s| !s.is_empty()),
            })
            .collect())
    }

    /// The Actions run a check belongs to (`…/actions/runs/<id>/…`).
    pub fn run_id(url: &str) -> Option<&str> {
        url.split("/actions/runs/").nth(1)?.split('/').next()
    }

    /// The failed jobs' log, tail end.
    pub async fn failed_log(&self, check: &CiCheck) -> Option<String> {
        let id = Self::run_id(check.url.as_deref()?)?;
        let out = self
            .gh(&["run", "view", id, "--log-failed"], None)
            .await
            .ok()?;
        Some(ci_log(&out))
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
    use otter_core::task::{Criterion, Delivery, Evidence};

    fn task() -> Task {
        let mut f = Task::new("CSV".into(), "Export".into(), chrono::Utc::now());
        f.acceptance.push(Criterion {
            id: "ac_1".into(),
            text: "exports".into(),
            met: Some(true),
            evidence: vec![],
            how: otter_core::task::Proof::Diff,
        });
        f.verify_command = Some("make test".into());
        f.evidence.push(Evidence {
            id: otter_core::EvidenceId::generate(),
            step_id: None,
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
    fn a_local_task_needs_only_its_checks_and_you() {
        let f = task();
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
    fn a_published_task_waits_for_green_ci() {
        let mut f = task();
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
        let mut f = task();
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

    /// A run of `make test` failing, as `gh run view --log-failed` prints
    /// it: the time, the run and the commit differ from one run to the next.
    fn failed_run(second: u32, took: &str, run: u64, sha: &str, got: u32) -> String {
        let t = |ms: u32| format!("2026-10-20T18:0{second}:1{ms}.4598260Z");
        [
            format!(
                "test\tRun make test\t\u{feff}{} ##[group]Run make test",
                t(0)
            ),
            format!("test\tRun make test\t{} \x1b[36;1mmake test\x1b[0m", t(1)),
            format!("test\tRun make test\t{} HEAD is now at {sha}", t(2)),
            format!("test\tRun make test\t{} AssertionError: 3 != {got}", t(3)),
            format!("test\tRun make test\t{} Ran 21 tests in {took}", t(4)),
            format!(
                "test\tRun make test\t{} see https://github.com/o/r/actions/runs/{run}/job/1",
                t(5)
            ),
            format!(
                "test\tRun make test\t{} ##[error]Process completed with exit code 1.",
                t(6)
            ),
        ]
        .join("\n")
    }

    #[test]
    fn a_ci_log_reads_without_times_or_colours() {
        let log = ci_log(&failed_run(1, "0.010s", 38677940243, "a512001d4b", 4));
        assert_eq!(
            log,
            "[test › Run make test]\n##[group]Run make test\nmake test\nHEAD is now at a512001d4b\n\
             AssertionError: 3 != 4\nRan 21 tests in 0.010s\n\
             see https://github.com/o/r/actions/runs/38677940243/job/1\n\
             ##[error]Process completed with exit code 1.\n"
        );
        // A long log keeps its end, by lines.
        let long: Vec<String> = (0..100).map(|i| format!("j\ts\tline {i}")).collect();
        let log = ci_log(&long.join("\n"));
        assert_eq!(log.lines().count(), LOG_LINES + 1);
        assert!(
            log.ends_with("line 99\n") && !log.contains("line 59\n"),
            "{log}"
        );
        // What isn't gh's shape stays as it is.
        assert_eq!(ci_log("plain\n"), "plain\n");
    }

    #[test]
    fn a_ci_log_keeps_no_credentials() {
        let log = ci_log(
            "test\tdeploy\t2026-10-20T18:00:00.1Z GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123",
        );
        assert!(!log.contains("ghp_abcdefghij"), "{log}");
        assert!(
            !tail(
                "fatal: https://x:ghp_abcdefghijklmnopqrstuvwxyz0123@github.com/o/r",
                500
            )
            .contains("ghp_abcdefghij")
        );
    }

    #[test]
    fn the_same_ci_failure_is_the_same_whatever_its_times() {
        let first = ci_log(&failed_run(1, "0.010s", 38677940243, "a512001d4b", 4));
        let again = ci_log(&failed_run(7, "0.1s", 38677999999, "b2515520aa", 4));
        assert_ne!(first, again);
        assert!(same_ci_failure(&first, &again));
        // Kept before D-139: as gh printed it.
        assert!(same_ci_failure(
            &failed_run(1, "0.010s", 38677940243, "a512001d4b", 4),
            &failed_run(2, "1.5s", 38677940244, "c0ffee1234", 4)
        ));
        assert!(same_ci_failure("Time: 1.234 s\n", "Time: 12 s\n"));
        assert!(same_ci_failure(
            "--- FAIL: TestX (0.00s)\n",
            "--- FAIL: TestX (0.31s)\n"
        ));
        // Another failure isn't.
        let other = ci_log(&failed_run(7, "0.1s", 38677999999, "b2515520aa", 5));
        assert!(!same_ci_failure(&first, &other));
        assert!(!same_ci_failure(&first, "test: exit 1\n"));
    }

    #[test]
    fn what_varies_between_runs_is_set_aside() {
        for (a, b) in [
            // npm's log file, named by its time.
            (
                "npm ERR! A complete log of this run can be found in: /home/runner/.npm/_logs/2026-10-20T18_00_00_123Z-debug-0.log",
                "npm ERR! A complete log of this run can be found in: /home/runner/.npm/_logs/2026-10-21T09_12_44_907Z-debug-0.log",
            ),
            // Structured logs and Python's logging.
            (
                r#"time="2026-10-20T18:00:00Z" level=error msg="no db""#,
                r#"time="2026-10-21T00:00:01Z" level=error msg="no db""#,
            ),
            (
                "2026-10-20 18:00:00,123 ERROR app: no db",
                "2026-10-21 00:00:01,456 ERROR app: no db",
            ),
            (
                "[2026-10-20 23:59:59] ERROR x",
                "[2026-10-21 00:00:03] ERROR x",
            ),
            ("at 18:00:00Z: refused", "at 18:01:12Z: refused"),
            // Durations, Go's over a minute too.
            (
                "--- FAIL: TestSlow (1m02.34s)",
                "--- FAIL: TestSlow (1m05.11s)",
            ),
            ("Ran 21 tests in 0.010s", "Ran 21 tests in 1.5s"),
            ("Time: 1.234 s", "Time: 12 s"),
            (
                "1 failed in 65.23s (0:01:05)",
                "1 failed in 61.02s (0:01:01)",
            ),
            // Where in a file: a fix moves lines.
            (
                "thread 'x' panicked at src/lib.rs:42:9:",
                "thread 'x' panicked at src/lib.rs:44:9:",
            ),
            (
                r#"  File "/w/app.py", line 42, in handler"#,
                r#"  File "/w/app.py", line 47, in handler"#,
            ),
            (
                "tests/test_ledger.py:42: AssertionError",
                "tests/test_ledger.py:58: AssertionError",
            ),
            // Runs, processes, commits.
            (
                "see https://github.com/o/r/actions/runs/38677940243/job/1",
                "see https://github.com/o/r/actions/runs/38677999999/job/1",
            ),
            ("HEAD is now at a512001d4b", "HEAD is now at b2515520aa"),
        ] {
            assert!(same_ci_failure(a, b), "{a}\n{b}");
        }
    }

    #[test]
    fn another_failure_is_another() {
        for (a, b) in [
            ("AssertionError: 3 != 4", "AssertionError: 3 != 5"),
            (
                "Ran 21 tests in 0.010s\nFAILED (failures=1)",
                "Ran 22 tests in 0.010s\nFAILED (failures=1)",
            ),
            ("test_parse ... FAIL", "test_total ... FAIL"),
            (
                "thread 'x' panicked at src/lib.rs:42:9:",
                "thread 'x' panicked at src/main.rs:42:9:",
            ),
        ] {
            assert!(!same_ci_failure(a, b), "{a}\n{b}");
        }
        // Words that look like numbers stay words.
        assert!(!same_ci_failure("test12345 failed", "test54321 failed"));
    }

    #[test]
    fn a_secret_at_a_lines_cut_is_gone_whole() {
        let pad = "x".repeat(LOG_LINE - 15);
        let log = ci_log(&format!(
            "j\ts\t{pad} https://bot:ghp_abcdefghijklmnopqrstuvwxyz0123@github.com/o/r"
        ));
        assert!(!log.contains("ghp_"), "{log}");
        assert!(log.lines().all(|l| l.chars().count() <= LOG_LINE));
    }
}
