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
/// Why the pull request and CI gates don't apply to a task with nothing
/// of its own to publish (D-166).
const NOTHING_TO_PUBLISH: &str = "nothing to publish";
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

/// GitHub refused a pull request because the branch has no commit its
/// base doesn't (`No commits between main and <branch>`): there is nothing
/// to publish, which isn't a failure (D-166).
pub fn no_commits_between(error: &str) -> bool {
    error.to_lowercase().contains("no commits between")
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
    without_colours(&c[start..].iter().collect::<String>())
}

/// `text` without a terminal's colours and cursor moves: `ESC [`, its
/// parameters and a final byte. gh writes the escape as `^[` (caret
/// notation) where the log isn't going to a terminal, so that form goes
/// too; any other `ESC` is dropped.
pub fn without_colours(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < c.len() {
        let escape = match c[i] {
            '\x1b' => Some(1),
            '^' if c.get(i + 1) == Some(&'[') && c.get(i + 2) == Some(&'[') => Some(2),
            _ => None,
        };
        let Some(n) = escape else {
            out.push(c[i]);
            i += 1;
            continue;
        };
        let csi = i + n;
        if c.get(csi) != Some(&'[') {
            i = csi;
            continue;
        }
        // Parameters and intermediates, then the final byte.
        let end = c[csi + 1..]
            .iter()
            .position(|c| !('\x20'..='\x3f').contains(c))
            .map(|k| csi + 1 + k);
        match end {
            Some(e) if ('@'..='~').contains(&c[e]) => i = e + 1,
            // Not a sequence after all: an escape goes, `^[` stays.
            _ => {
                if n == 2 {
                    out.extend(&c[i..csi]);
                }
                i = csi;
            }
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
    // A word: a long number, a hash, an address or a UUID, else maybe a
    // duration.
    let word = &c[i..i + c[i..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count()];
    let digits = word.iter().filter(|c| c.is_ascii_digit()).count();
    let hex = |w: &[char]| w.iter().all(char::is_ascii_hexdigit);
    // 8-4-4-4-12 hex digits.
    let uuid = [8, 4, 4, 4, 12].iter().try_fold(i, |k, &n| {
        c.get(k..k + n).filter(|g| hex(g))?;
        let dash = n != 12;
        (!dash || c.get(k + n) == Some(&'-')).then_some(k + n + usize::from(dash))
    });
    if uuid.is_some_and(|end| !c.get(end).is_some_and(char::is_ascii_alphanumeric)) {
        return 36;
    }
    if (digits == word.len() && digits >= 5)
        || (word.len() >= 7 && digits > 0 && digits < word.len() && hex(word))
        || (word.len() > 2 && word[..2] == ['0', 'x'] && hex(&word[2..]))
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

/// Where in a file or a program: `lib.rs:42:9`, `test_x.py:42:`,
/// `line 42`, `goroutine 18`.
fn position(c: &[char], i: usize) -> usize {
    let after = |w: &str| {
        let n = w.len();
        i >= n
            && c[i - n..i]
                .iter()
                .collect::<String>()
                .eq_ignore_ascii_case(w)
            && (i == n || !c[i - n - 1].is_ascii_alphabetic())
    };
    let line = after("line ") || after("goroutine ");
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
    /// The task has nothing of its own to publish (D-166).
    pub nothing_to_publish: bool,
    /// No CI check has started long after the last push (D-184).
    pub no_ci_after_push: bool,
}

/// GitHub's word for a pull request that conflicts with its base.
pub const CONFLICTING: &str = "CONFLICTING";

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
        Some(d) if d.pr_url.is_some() && d.mergeable.as_deref() == Some(CONFLICTING) => gate(
            "Pull request",
            GateStatus::Failed,
            format!("conflicts with {}", base_of(d)),
        ),
        Some(d) if d.pr_url.is_some() => gate(
            "Pull request",
            GateStatus::Passed,
            d.pr_url.clone().unwrap_or_default(),
        ),
        _ if cx.nothing_to_publish => gate(
            "Pull request",
            GateStatus::NotApplicable,
            NOTHING_TO_PUBLISH,
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
            } else if d.ci.is_empty() && d.mergeable.as_deref() == Some(CONFLICTING) {
                // GitHub runs no `pull_request` workflow on it (D-184).
                gate(
                    "CI",
                    GateStatus::Failed,
                    format!("can't run: the branch conflicts with {}", base_of(d)),
                )
            } else if d.ci.is_empty() && cx.has_ci && cx.no_ci_after_push {
                // No count of minutes in it: the gate would change, and the
                // task be written, every minute.
                gate(
                    "CI",
                    GateStatus::Pending,
                    "no check has started 10 min after the push",
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
        _ if cx.nothing_to_publish => gate("CI", GateStatus::NotApplicable, NOTHING_TO_PUBLISH),
        _ => gate("CI", GateStatus::Pending, "after the pull request"),
    });
    // No gate for the developer's acceptance any more (D-199): the task
    // ends when these pass.
    out
}

/// The branch a pull request goes into, to name it.
fn base_of(d: &otter_core::task::Delivery) -> String {
    d.base
        .clone()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "its base".into())
}

/// Every gate is clear: the work is delivered (D-199). An older task's
/// acceptance gate doesn't count.
pub fn all_clear(gates: &[Gate]) -> bool {
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

/// A tool's failure as Otter says it: the command and the end of what it
/// printed, without credentials in either (invariant 13): it becomes a
/// task's reason and message.
fn failed(program: &str, args: &[&str], stderr: &str, n: usize) -> String {
    otter_core::redact::redact(&format!(
        "{program} {}: {}",
        args.join(" "),
        tail(stderr.trim(), n)
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
        Ok(self.output(args).await?.trim().to_owned())
    }

    /// The remote's default branch (`origin/HEAD`), if Git knows it.
    pub async fn default_branch(&self) -> Option<String> {
        let head = self
            .git(&["rev-parse", "--abbrev-ref", "origin/HEAD"])
            .await
            .ok()?;
        head.strip_prefix("origin/")
            .filter(|b| !b.is_empty())
            .map(str::to_owned)
    }

    /// Where this repository comes from (`origin`), and the branch checked
    /// out when `origin` has it too (D-192): what a workspace of its own is
    /// made from. `None` without a remote.
    pub async fn origin_and_branch(&self) -> Option<(String, Option<String>)> {
        let url = self.git(&["remote", "get-url", "origin"]).await.ok()?;
        if url.is_empty() {
            return None;
        }
        let branch = self
            .git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .ok()
            .filter(|b| !b.is_empty() && b != "HEAD");
        let tracked = match &branch {
            Some(b) => self
                .git(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/origin/{b}"),
                ])
                .await
                .is_ok(),
            None => false,
        };
        Some((url, branch.filter(|_| tracked)))
    }

    /// What Git printed, as it printed it (a status line starts with a
    /// space).
    async fn output(&self, args: &[&str]) -> Result<String> {
        let git = which("git", self.env).ok_or_else(|| anyhow!("git is not installed"))?;
        let (ok, out, err) = run(&git, args, self.root, self.env, None).await?;
        if !ok {
            bail!(failed("git", args, &err, 500));
        }
        Ok(out)
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

    /// The other worktrees of this repository (`git worktree list`), as
    /// directories on this host. Empty when Git can't say.
    pub async fn worktrees(&self) -> Vec<std::path::PathBuf> {
        let Ok(out) = self.output(&["worktree", "list", "--porcelain"]).await else {
            return vec![];
        };
        // One block per worktree; the bare repository Otter's workspaces
        // share is listed first, and is not a work tree (D-174).
        let all: Vec<std::path::PathBuf> = out
            .split("\n\n")
            .filter(|b| !b.lines().any(|l| l == "bare"))
            .filter_map(|b| b.lines().find_map(|l| l.strip_prefix("worktree ")))
            .map(std::path::PathBuf::from)
            .collect();
        // Only when the repository knows this directory as a worktree: Git
        // run in a folder that is none (an empty workspace in a checkout), or
        // whose `.git` file the agent pointed elsewhere, lists someone else's.
        let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if !all.iter().any(|p| real(p) == real(self.root)) {
            return vec![];
        }
        all.into_iter()
            .filter(|p| real(p) != real(self.root))
            .collect()
    }

    pub async fn head(&self) -> Result<String> {
        self.git(&["rev-parse", "HEAD"]).await
    }

    /// Whether HEAD is `rev` or behind it; `None` when `rev` isn't known
    /// here (D-189).
    pub async fn within(&self, rev: &str) -> Option<bool> {
        self.git(&["cat-file", "-e", &format!("{rev}^{{commit}}")])
            .await
            .ok()?;
        Some(
            self.git(&["merge-base", "--is-ancestor", "HEAD", rev])
                .await
                .is_ok(),
        )
    }

    /// Whether HEAD is `rev` or descends from it.
    pub async fn descends_from(&self, rev: &str) -> bool {
        self.git(&["merge-base", "--is-ancestor", rev, "HEAD"])
            .await
            .is_ok()
    }

    /// How many commits HEAD has that `origin`'s branch `base` doesn't, as
    /// last fetched. `None`: no such branch known here. A local `<base>` is
    /// no stand-in: in a worktree of a shared repository it can be anywhere.
    pub async fn ahead_of(&self, base: &str) -> Option<u64> {
        self.git(&["rev-list", "--count", &format!("origin/{base}..HEAD")])
            .await
            .ok()?
            .parse()
            .ok()
    }

    /// Whether anything is changed or new, secrets' names included.
    pub async fn dirty(&self) -> bool {
        self.git(&["status", "--porcelain=v1", "--untracked-files=all"])
            .await
            .is_ok_and(|s| !s.trim().is_empty())
    }

    /// Whether `origin` has the branch `name` (as last fetched).
    pub async fn remote_branch_exists(&self, name: &str) -> bool {
        self.git(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/remotes/origin/{name}"),
        ])
        .await
        .is_ok()
    }

    /// The last commit's subject.
    pub async fn subject(&self) -> Option<String> {
        self.git(&["log", "-1", "--format=%s"])
            .await
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// What the branch changes against its base, by file.
    pub async fn diffstat(&self, base: Option<&str>) -> Option<String> {
        let base = base.unwrap_or("origin/HEAD");
        self.git(&["diff", "--stat", &format!("{base}...HEAD")])
            .await
            .ok()
    }

    /// Changes a commit would take: anything changed or new but files that
    /// may hold secrets (by name, as `work_state` reads them). An error when
    /// Git can't say (an index held by another, an I/O error): not "nothing".
    pub async fn committable(&self) -> Result<bool> {
        let mut args = vec![
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--",
            ".",
        ];
        let excluded = crate::work_state::secret_pathspecs();
        args.extend(excluded.iter().map(String::as_str));
        Ok(!self.git(&args).await?.is_empty())
    }

    /// Files changed or new that a commit leaves out because they may hold
    /// secrets (by name).
    pub async fn left_out(&self) -> Result<Vec<String>> {
        let status = self
            .output(&["status", "--porcelain=v1", "--untracked-files=all"])
            .await?;
        Ok(status
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
            .collect())
    }

    /// Commit what the work left uncommitted, as `message` (D-068): the new
    /// HEAD, and the files left out because they may hold secrets. `None`:
    /// nothing to commit. Without an identity of the host's own, the commit
    /// is Otter's.
    pub async fn commit_work(&self, message: &str) -> Result<Option<(String, Vec<String>)>> {
        let left_out = self.left_out().await?;
        if !self.committable().await? {
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
    /// `owner/name`, when `root` isn't the repository (a watch, D-183).
    pub repo: Option<String>,
}

/// An Actions run as GitHub says it (D-193).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunView {
    pub name: String,
    pub title: String,
    /// `queued`, `in_progress`, `completed`, …
    pub status: String,
    /// Once completed: `success`, `failure`, `cancelled`, `skipped`,
    /// `timed_out`, …
    pub conclusion: String,
    pub event: String,
    pub branch: String,
    pub commit: String,
    pub created_at: String,
    pub updated_at: String,
    pub url: String,
    pub jobs: Vec<JobView>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobView {
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub failed_steps: Vec<String>,
}

/// What a GitHub link points at (D-193).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GithubLink {
    /// An Actions run (or one of its jobs).
    Run {
        repo: String,
        id: String,
        job: Option<String>,
    },
    PullRequest {
        repo: String,
        number: u64,
    },
}

impl GithubLink {
    /// `https://github.com/<owner>/<repo>/actions/runs/<id>[/job/<id>]` or
    /// `…/pull/<n>`; anything else is `None`.
    pub fn parse(url: &str) -> Option<GithubLink> {
        let url = url.trim();
        let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
        let rest = rest.split(['?', '#']).next()?;
        let mut parts = rest.split('/').filter(|p| !p.is_empty());
        if parts.next()? != "github.com" {
            return None;
        }
        let (owner, name) = (parts.next()?, parts.next()?);
        let ok = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if !ok(owner) || !ok(name) {
            return None;
        }
        let repo = format!("{owner}/{name}");
        let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        match (parts.next()?, parts.next()?) {
            ("actions", "runs") => {
                let id = parts.next().filter(|s| digits(s))?.to_owned();
                let job = match (parts.next(), parts.next()) {
                    (Some("job"), Some(j)) if digits(j) => Some(j.to_owned()),
                    _ => None,
                };
                Some(GithubLink::Run { repo, id, job })
            }
            ("pull", n) => Some(GithubLink::PullRequest {
                repo,
                number: n.parse().ok()?,
            }),
            _ => None,
        }
    }

    pub fn repo(&self) -> &str {
        match self {
            GithubLink::Run { repo, .. } | GithubLink::PullRequest { repo, .. } => repo,
        }
    }
}

/// What GitHub says of a pull request now (D-184).
#[derive(Clone, Debug, PartialEq)]
pub struct PrState {
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN` (GitHub still working it out).
    pub mergeable: String,
    pub head: String,
    pub base: String,
    /// When it was merged, if it was (D-189).
    pub merged_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl<'a> GitHub<'a> {
    pub fn find(root: &'a Path, env: &'a EnvMap) -> Option<GitHub<'a>> {
        Some(GitHub {
            gh: which("gh", env)?,
            root,
            env,
            repo: None,
        })
    }

    /// `args`, with `--repo` for a pull request or run when `root` isn't
    /// the repository.
    fn with_repo<'b>(&'b self, args: &[&'b str]) -> Vec<&'b str> {
        let mut all = args.to_vec();
        if let Some(r) = self.repo.as_deref()
            && matches!(args.first(), Some(&"pr") | Some(&"run"))
        {
            all.extend(["--repo", r]);
        }
        all
    }

    async fn gh(&self, args: &[&str], input: Option<&str>) -> Result<String> {
        let args = self.with_repo(args);
        let (ok, out, err) = run(&self.gh, &args, self.root, self.env, input).await?;
        if !ok {
            bail!(failed("gh", &args, &err, 500));
        }
        Ok(out)
    }

    /// What GitHub says of pull request `pr` now: open or not, whether it
    /// can be merged, its head and base (D-184), and when it was merged
    /// (D-189).
    pub async fn pr_state(&self, pr: u64) -> Result<PrState> {
        let n = pr.to_string();
        let out = self
            .gh(
                &[
                    "pr",
                    "view",
                    &n,
                    "--json",
                    "state,mergeable,headRefOid,baseRefName,mergedAt",
                ],
                None,
            )
            .await?;
        let v: Value = serde_json::from_str(&out).context("reading gh pr view")?;
        let field = |k: &str, or: &str| v[k].as_str().unwrap_or(or).to_owned();
        Ok(PrState {
            state: field("state", "OPEN"),
            mergeable: field("mergeable", "UNKNOWN"),
            head: field("headRefOid", ""),
            base: field("baseRefName", ""),
            merged_at: v["mergedAt"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc)),
        })
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
        self.create(branch, base, title, body).await
    }

    /// Open a pull request, never touching one that exists.
    pub async fn create(
        &self,
        branch: &str,
        base: Option<&str>,
        title: &str,
        body: &str,
    ) -> Result<(u64, String)> {
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
        let n = pr.to_string();
        let args = self.with_repo(&["pr", "checks", &n, "--json", "name,bucket,link,description"]);
        let (_, out, err) = run(&self.gh, &args, self.root, self.env, None).await?;
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

    /// An Actions run as GitHub says it now (D-193): its state, and its
    /// jobs with the steps that failed.
    pub async fn run_view(&self, id: &str) -> Result<RunView> {
        let out = self
            .gh(
                &[
                    "run",
                    "view",
                    id,
                    "--json",
                    "name,displayTitle,status,conclusion,event,headBranch,headSha,createdAt,updatedAt,url,jobs",
                ],
                None,
            )
            .await?;
        let v: Value = serde_json::from_str(&out).context("reading gh run view")?;
        let field = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_owned();
        Ok(RunView {
            name: field(&v, "name"),
            title: field(&v, "displayTitle"),
            status: field(&v, "status"),
            conclusion: field(&v, "conclusion"),
            event: field(&v, "event"),
            branch: field(&v, "headBranch"),
            commit: field(&v, "headSha").chars().take(10).collect(),
            created_at: field(&v, "createdAt"),
            updated_at: field(&v, "updatedAt"),
            url: field(&v, "url"),
            jobs: v["jobs"]
                .as_array()
                .map(|jobs| {
                    jobs.iter()
                        .map(|j| JobView {
                            name: field(j, "name"),
                            status: field(j, "status"),
                            conclusion: field(j, "conclusion"),
                            failed_steps: j["steps"]
                                .as_array()
                                .map(|steps| {
                                    steps
                                        .iter()
                                        .filter(|s| {
                                            matches!(
                                                s["conclusion"].as_str(),
                                                Some("failure" | "timed_out" | "cancelled")
                                            )
                                        })
                                        .map(|s| field(s, "name"))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// The failed jobs' log of run `id`, cleaned as CI logs are (D-139).
    pub async fn run_failed_log(&self, id: &str) -> Option<String> {
        let out = self
            .gh(&["run", "view", id, "--log-failed"], None)
            .await
            .ok()?;
        Some(ci_log(&out)).filter(|l| !l.trim().is_empty())
    }

    /// A pull request as GitHub says it now (D-193): title, state,
    /// mergeability, branches, link.
    pub async fn pr_view(&self, pr: u64) -> Result<Value> {
        let n = pr.to_string();
        let out = self
            .gh(
                &[
                    "pr",
                    "view",
                    &n,
                    "--json",
                    "title,state,mergeable,baseRefName,headRefName,url,isDraft",
                ],
                None,
            )
            .await?;
        serde_json::from_str(&out).context("reading gh pr view")
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

    /// A workspace is a worktree of a bare backing repository, which `git
    /// worktree list` shows first: it is not the task's to edit (D-174).
    #[tokio::test]
    async fn the_backing_repository_is_not_one_of_the_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        let (src, base) = (dir.path().join("src"), dir.path().join("base"));
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        git(&src, &["commit", "-q", "--allow-empty", "-m", "x"]);
        git(dir.path(), &["clone", "-q", "--bare", "src", "base"]);
        git(
            &base,
            &["worktree", "add", "-q", "-b", "wa", a.to_str().unwrap()],
        );
        git(
            &base,
            &["worktree", "add", "-q", "-b", "wb", b.to_str().unwrap()],
        );
        let env: EnvMap = std::env::vars().collect();
        let trees = Git {
            root: &a,
            env: &env,
        }
        .worktrees()
        .await;
        let real = |p: &Path| p.canonicalize().unwrap();
        assert_eq!(
            trees.iter().map(|t| real(t)).collect::<Vec<_>>(),
            [real(&b)],
            "the sibling, not the backing repository and not itself"
        );
        // A folder that is no worktree, in one: Git lists the checkout around
        // it, which is not the task's.
        let inner = a.join("sub");
        std::fs::create_dir(&inner).unwrap();
        assert_eq!(
            Git {
                root: &inner,
                env: &env
            }
            .worktrees()
            .await
            .len(),
            0
        );
        let other = dir.path().join("elsewhere");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(
            other.join(".git"),
            format!("gitdir: {}\n", src.join(".git").display()),
        )
        .unwrap();
        assert_eq!(
            Git {
                root: &other,
                env: &env
            }
            .worktrees()
            .await
            .len(),
            0
        );
    }

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
            mergeable: None,
            ci_read_at: None,
        }
    }

    /// D-184: a pull request that conflicts gets no CI; that isn't "running".
    #[test]
    fn a_conflicting_pull_request_fails_its_gates_instead_of_waiting() {
        let mut f = task();
        let mut d = delivery(&[]);
        d.base = Some("main".into());
        d.mergeable = Some(CONFLICTING.into());
        f.delivery = Some(d);
        let cx = GateContext {
            publishable: true,
            has_ci: true,
            browser_checks: false,
            ci_settling: false,
            nothing_to_publish: false,
            no_ci_after_push: false,
        };
        let g = gates(&f, &cx);
        assert_eq!(status(&g, "Pull request"), GateStatus::Failed);
        let ci = g.iter().find(|g| g.name == "CI").unwrap();
        assert_eq!(ci.status, GateStatus::Failed);
        assert_eq!(
            ci.detail.as_deref(),
            Some("can't run: the branch conflicts with main")
        );
        // Mergeable, no checks long after the push: said, still waited on.
        f.delivery.as_mut().unwrap().mergeable = Some("MERGEABLE".into());
        let late = GateContext {
            no_ci_after_push: true,
            ..cx
        };
        let g = gates(&f, &late);
        assert_eq!(status(&g, "Pull request"), GateStatus::Passed);
        let ci = g.iter().find(|g| g.name == "CI").unwrap();
        assert_eq!(ci.status, GateStatus::Pending);
        assert_eq!(
            ci.detail.as_deref(),
            Some("no check has started 10 min after the push")
        );
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
                nothing_to_publish: false,
                no_ci_after_push: false,
            },
        );
        assert_eq!(status(&g, "Requirements met"), GateStatus::Passed);
        assert_eq!(status(&g, "Local tests"), GateStatus::Passed);
        assert_eq!(status(&g, "Pull request"), GateStatus::NotApplicable);
        assert_eq!(status(&g, "CI"), GateStatus::NotApplicable);
        // No acceptance gate (D-199): delivered is done.
        assert!(!g.iter().any(|x| x.name == ACCEPTANCE_GATE));
        assert!(all_clear(&g));
    }

    #[test]
    fn a_published_task_waits_for_green_ci() {
        let mut f = task();
        let cx = GateContext {
            publishable: true,
            has_ci: true,
            browser_checks: true,
            ci_settling: false,
            nothing_to_publish: false,
            no_ci_after_push: false,
        };
        let g = gates(&f, &cx);
        assert_eq!(status(&g, "Pull request"), GateStatus::Pending);
        assert_eq!(status(&g, "Browser evidence"), GateStatus::Pending);
        assert!(!all_clear(&g));

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
            mergeable: None,
            ci_read_at: None,
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
                nothing_to_publish: false,
                no_ci_after_push: false,
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

    /// gh writes a log's colours as `^[[…m` where it isn't going to a
    /// terminal: those go too, as escapes do (B11's note; D-139).
    #[test]
    fn colours_go_in_either_spelling() {
        let log = ci_log(
            "check\tRun make check\t2026-10-21T15:18:50.1Z ^[[36;1mmake check^[[0m\n\
             check\tRun make check\t2026-10-21T15:18:51.1Z \x1b[31mError 1\x1b[0m\n",
        );
        assert_eq!(
            log, "[check › Run make check]\nmake check\nError 1\n",
            "{log:?}"
        );
        // Not a colour: kept.
        assert_eq!(without_colours("x ^[ y [1] ^[["), "x ^[ y [1] ^[[");
        assert_eq!(without_colours("a\x1bb"), "ab");
        assert_eq!(without_colours("^[[2K^[[1;32mok"), "ok");
    }

    /// What Git or gh said, and the command it was, become a task's reason
    /// without credentials (invariant 13).
    #[test]
    fn a_tools_failure_says_no_secret() {
        let said = failed(
            "git",
            &[
                "push",
                "https://bob:ghp_abcdefghijklmnopqrstuvwxyz0123456789@github.com/o/r",
            ],
            "remote: Invalid username or token ghp_abcdefghijklmnopqrstuvwxyz0123456789\n",
            500,
        );
        assert!(said.starts_with("git push https://"), "{said}");
        assert!(!said.contains("ghp_abcdef"), "{said}");
        assert!(!said.contains("bob:"), "{said}");
    }

    #[test]
    fn no_commits_is_told_apart() {
        assert!(no_commits_between(
            "gh pr create --head otterd/r03br: pull request create failed: GraphQL: No commits between main and otterd/r03br (createPullRequest)"
        ));
        assert!(!no_commits_between("pull request create failed: HTTP 422"));
    }

    #[test]
    fn github_links_say_what_they_point_at() {
        assert_eq!(
            GithubLink::parse(
                "https://github.com/sugerio/marketplace-service/actions/runs/39824362436"
            ),
            Some(GithubLink::Run {
                repo: "sugerio/marketplace-service".into(),
                id: "39824362436".into(),
                job: None
            })
        );
        assert_eq!(
            GithubLink::parse("github.com/o/r/actions/runs/12/job/34?pr=1"),
            Some(GithubLink::Run {
                repo: "o/r".into(),
                id: "12".into(),
                job: Some("34".into())
            })
        );
        assert_eq!(
            GithubLink::parse("https://github.com/o/r/pull/7#issuecomment-1"),
            Some(GithubLink::PullRequest {
                repo: "o/r".into(),
                number: 7
            })
        );
        for bad in [
            "https://gitlab.com/o/r/pull/7",
            "https://github.com/o/r/issues/7",
            "https://github.com/o/r/actions/runs/--repo",
            "https://github.com/o;rm/r/pull/7",
            "not a link",
        ] {
            assert_eq!(GithubLink::parse(bad), None, "{bad}");
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
            // Addresses, goroutines, records.
            (
                "AssertionError: <Foo object at 0x7f3a2b1c4d50> != 3",
                "AssertionError: <Foo object at 0x7f99e0a1b2c3> != 3",
            ),
            ("goroutine 18 [running]:", "goroutine 7 [running]:"),
            (
                "no order 550e8400-e29b-41d4-a716-446655440000",
                "no order 123e4567-e89b-12d3-a456-426614174000",
            ),
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
        assert!(!same_ci_failure("pipeline 3 failed", "pipeline 4 failed"));
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
