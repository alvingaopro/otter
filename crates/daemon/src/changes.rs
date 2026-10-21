//! A workspace's Git work, for the developer (D-102): the branch, commits
//! ahead of and behind its base, the changed files with their line counts,
//! and one file's diff. Read-only and never touching the index (it's the
//! agent's); scoped to the workspace root (`-- .`), which may be a directory
//! inside a larger repository. Verification reads the same work its own way
//! (`work_state`).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use otter_core::{Workspace, WorkspaceSource};
use otter_protocol::work::{
    ChangeKind, ChangedFile, FileDiff, MAX_CHANGED_FILES, MAX_DIFF_BYTES, Published, PullRequest,
    SessionPort, WorkspaceChanges, WorkspacePublish,
};

use crate::daemon::{Daemon, RpcResult};
use crate::delivery::run;
use crate::env::{EnvMap, which};
use crate::work_state::merge_base;
use otter_protocol::RpcError;

/// New files whose lines are counted, up to this size each, this many of
/// them and this much in all; past that they are listed without counts, as
/// a binary file is (an un-ignored `node_modules` is not read through on
/// every look).
const COUNTED_BYTES: u64 = 1 << 20;
const COUNTED_FILES: usize = 2000;
const COUNTED_TOTAL: u64 = 64 << 20;
/// Most context `workspace.diff` gives: more than any file it can return.
const MAX_CONTEXT: u32 = 1_000_000;

struct Git<'a> {
    git: PathBuf,
    root: &'a Path,
    env: &'a EnvMap,
}

impl Git<'_> {
    /// Exit status and stdout. Paths are literal: a file named `:(top)` is
    /// a file.
    async fn run(&self, args: &[&str]) -> (bool, String) {
        let mut all = vec!["--literal-pathspecs", "-c", "core.quotepath=off"];
        all.extend_from_slice(args);
        match run(&self.git, &all, self.root, self.env, None).await {
            Ok((ok, out, _)) => (ok, out),
            Err(_) => (false, String::new()),
        }
    }

    async fn out(&self, args: &[&str]) -> Option<String> {
        let (ok, out) = self.run(args).await;
        ok.then(|| out.trim().to_owned()).filter(|s| !s.is_empty())
    }
}

async fn open<'a>(root: &'a Path, env: &'a EnvMap) -> Option<Git<'a>> {
    let git = Git {
        git: which("git", env)?,
        root,
        env,
    };
    let (inside, out) = git.run(&["rev-parse", "--is-inside-work-tree"]).await;
    (inside && out.trim() == "true").then_some(git)
}

/// What the changes are counted from: the merge base with `base` (or the
/// branch's upstream), named; else HEAD; else nothing (no commits yet).
async fn starting_point(g: &Git<'_>, base: Option<&str>) -> Option<(Option<String>, String)> {
    let head = g.out(&["rev-parse", "--verify", "-q", "HEAD"]).await?;
    let upstream = match base {
        Some(_) => None,
        None => {
            g.out(&[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ])
            .await
        }
    };
    match merge_base(&g.git, g.root, g.env, base.or(upstream.as_deref())).await {
        Some((name, rev)) => Some((Some(name), rev)),
        None => Some((None, head)),
    }
}

/// `workspace.changes` for the work in `root`. `base`: the branch the work
/// started from, if known.
pub(crate) async fn changes(root: &Path, env: &EnvMap, base: Option<&str>) -> WorkspaceChanges {
    let Some(g) = open(root, env).await else {
        return WorkspaceChanges::default();
    };
    let mut out = WorkspaceChanges {
        git: true,
        branch: g.out(&["symbolic-ref", "--short", "-q", "HEAD"]).await,
        ..Default::default()
    };
    let from = starting_point(&g, base).await;
    if let Some((Some(name), _)) = &from {
        out.base = Some(name.clone());
        let range = format!("{name}...HEAD");
        if let Some(counts) = g
            .out(&["rev-list", "--left-right", "--count", &range])
            .await
        {
            let mut n = counts.split_whitespace().map(|n| n.parse().unwrap_or(0));
            out.behind = n.next().unwrap_or(0);
            out.ahead = n.next().unwrap_or(0);
        }
    }
    // Work no remote has, whatever the base: after a push (the branch's
    // remote-tracking ref moves) it is gone, a new branch's included.
    out.unpushed = g
        .out(&["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .await
        .and_then(|n| n.parse().ok());
    // Since the starting point, committed or not; with no commits yet, what
    // is staged.
    let (numstat, status, uncommitted) = match &from {
        Some((_, rev)) => {
            let numstat = g
                .run(&[
                    "diff",
                    "--relative",
                    "-M",
                    "--numstat",
                    "-z",
                    rev,
                    "--",
                    ".",
                ])
                .await
                .1;
            let status = g
                .run(&[
                    "diff",
                    "--relative",
                    "-M",
                    "--name-status",
                    "-z",
                    rev,
                    "--",
                    ".",
                ])
                .await
                .1;
            let uncommitted = g
                .run(&["diff", "--relative", "--name-only", "-z", "HEAD", "--", "."])
                .await
                .1;
            (numstat, status, uncommitted)
        }
        None => {
            let numstat = g
                .run(&[
                    "diff",
                    "--cached",
                    "--relative",
                    "-M",
                    "--numstat",
                    "-z",
                    "--",
                    ".",
                ])
                .await
                .1;
            let status = g
                .run(&[
                    "diff",
                    "--cached",
                    "--relative",
                    "-M",
                    "--name-status",
                    "-z",
                    "--",
                    ".",
                ])
                .await
                .1;
            (numstat, status.clone(), status)
        }
    };
    let uncommitted: HashSet<&str> = match &from {
        Some(_) => uncommitted.split('\0').filter(|p| !p.is_empty()).collect(),
        // Nothing is committed yet: everything is.
        None => parse_name_status(&uncommitted)
            .into_iter()
            .map(|(_, p)| p)
            .collect(),
    };
    let counts: BTreeMap<&str, (Option<u64>, Option<u64>)> = parse_numstat(&numstat)
        .into_iter()
        .map(|(p, a, d)| (p, (a, d)))
        .collect();
    let mut files: Vec<ChangedFile> = parse_name_status(&status)
        .into_iter()
        .map(|(change, path)| {
            let (insertions, deletions) = counts.get(path).copied().unwrap_or((None, None));
            ChangedFile {
                path: path.to_owned(),
                change,
                insertions,
                deletions,
                uncommitted: uncommitted.contains(path),
            }
        })
        .collect();
    let (_, untracked) = g
        .run(&[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])
        .await;
    let paths: Vec<String> = untracked
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    // Reading files blocks: off the runtime's threads.
    let dir = root.to_path_buf();
    let counted = tokio::task::spawn_blocking({
        let paths = paths.clone();
        move || count_new_files(&dir, &paths)
    })
    .await
    .unwrap_or_else(|_| vec![None; paths.len()]);
    for (path, lines) in paths.into_iter().zip(counted) {
        files.push(ChangedFile {
            path,
            change: ChangeKind::Untracked,
            insertions: lines,
            deletions: lines.map(|_| 0),
            uncommitted: true,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    out.insertions = files.iter().filter_map(|f| f.insertions).sum();
    out.deletions = files.iter().filter_map(|f| f.deletions).sum();
    if files.len() > MAX_CHANGED_FILES {
        out.more = (files.len() - MAX_CHANGED_FILES) as u32;
        files.truncate(MAX_CHANGED_FILES);
    }
    out.files = files;
    out
}

/// `workspace.diff`: one file, since the same starting point; a new file
/// against nothing.
pub(crate) async fn diff(
    root: &Path,
    env: &EnvMap,
    base: Option<&str>,
    path: &str,
    context: Option<u32>,
) -> FileDiff {
    let Some(g) = open(root, env).await else {
        return FileDiff::default();
    };
    let lines = format!("-U{}", context.unwrap_or(3).min(MAX_CONTEXT));
    let flags = ["--no-color", "--no-ext-diff", "--relative", lines.as_str()];
    let untracked = g
        .run(&[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            path,
        ])
        .await
        .1;
    let text = if !untracked.is_empty() {
        // Exits 1 when there's a difference, which there is.
        g.run(&[
            "diff",
            "--no-color",
            "--no-ext-diff",
            lines.as_str(),
            "--no-index",
            "--",
            "/dev/null",
            path,
        ])
        .await
        .1
    } else {
        match starting_point(&g, base).await {
            Some((_, rev)) => {
                let mut args = vec!["diff"];
                args.extend(flags);
                args.extend([rev.as_str(), "--", path]);
                g.run(&args).await.1
            }
            None => {
                let mut args = vec!["diff", "--cached"];
                args.extend(flags);
                args.extend(["--", path]);
                g.run(&args).await.1
            }
        }
    };
    bounded(text)
}

fn bounded(mut text: String) -> FileDiff {
    if text.len() <= MAX_DIFF_BYTES {
        return FileDiff {
            text,
            truncated: false,
        };
    }
    let mut cut = MAX_DIFF_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    FileDiff {
        text,
        truncated: true,
    }
}

/// The branch a Git workspace's work started from, as `merge_base` takes it.
fn base_of(ws: &Workspace) -> Option<String> {
    match &ws.source {
        WorkspaceSource::Git(g) => g
            .base
            .as_deref()
            .map(|b| b.trim_start_matches("origin/").to_owned()),
        _ => None,
    }
}

impl Daemon {
    pub(crate) async fn workspace_changes(&self, r: &str) -> RpcResult<WorkspaceChanges> {
        let ws = self.workspace_get(r).await?;
        let env = self.environments.base().clone();
        Ok(changes(Path::new(&ws.root), &env, base_of(&ws).as_deref()).await)
    }

    /// Push the workspace's branch, and open its pull request if asked
    /// (D-152). The developer asked for it, in a client: that is the
    /// allowance a task asks for (D-047). Nothing is committed for them, and
    /// a pull request that exists keeps its description.
    pub(crate) async fn workspace_publish(&self, p: WorkspacePublish) -> RpcResult<Published> {
        use crate::delivery::{Git, GitHub};
        let ws = self.workspace_get(&p.workspace).await?;
        self.not_being_deleted(&ws)?;
        if ws.state != otter_core::WorkspaceState::Ready {
            return Err(RpcError::conflict(format!(
                "workspace `{}` is not ready",
                ws.name
            )));
        }
        if !matches!(ws.source, WorkspaceSource::Git(_)) {
            return Err(RpcError::conflict(format!(
                "workspace `{}` is not a Git workspace",
                ws.name
            )));
        }
        let root = PathBuf::from(&ws.root);
        let env = self.env_for_launch(&ws).await?;
        let git = Git {
            root: &root,
            env: &env,
        };
        let Some(branch) = git.publishable_branch().await else {
            return Err(RpcError::conflict(
                "there is no branch to push, or no `origin` to push it to",
            ));
        };
        git.push(&branch)
            .await
            .map_err(|e| RpcError::internal(format!("pushing {branch}: {e:#}")))?;
        let mut out = Published {
            branch: branch.clone(),
            uncommitted: git.committable().await,
            pull_request: None,
            note: None,
        };
        if !p.pull_request {
            return Ok(out);
        }
        // Otter's own GitHub sign-in when it has one (D-133), else the host's.
        let gh_env = self.git_account.gh_env(&env);
        let Some(gh) = GitHub::find(&root, &gh_env) else {
            out.note = Some(
                "`gh` is not installed on this host: the branch is pushed, no pull request opened"
                    .into(),
            );
            return Ok(out);
        };
        if let Some((number, url)) = gh.find_pr(&branch).await {
            out.pull_request = Some(PullRequest {
                number,
                url,
                opened: false,
            });
            return Ok(out);
        }
        // A workspace on an existing branch has that branch for its base:
        // the pull request goes to the repository's default.
        let base = base_of(&ws).filter(|b| *b != branch);
        let title = match p.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => t.to_owned(),
            None => git.subject().await.unwrap_or_else(|| branch.clone()),
        };
        let (number, url) = gh
            .publish(
                &branch,
                base.as_deref(),
                &title,
                p.body.as_deref().unwrap_or(""),
            )
            .await
            .map_err(|e| RpcError::internal(format!("opening the pull request: {e:#}")))?;
        out.pull_request = Some(PullRequest {
            number,
            url,
            opened: true,
        });
        Ok(out)
    }

    pub(crate) async fn workspace_diff(
        &self,
        r: &str,
        path: &str,
        context: Option<u32>,
    ) -> RpcResult<FileDiff> {
        let ws = self.workspace_get(r).await?;
        let env = self.environments.base().clone();
        Ok(diff(
            Path::new(&ws.root),
            &env,
            base_of(&ws).as_deref(),
            path,
            context,
        )
        .await)
    }

    /// Ports listened on by a process in one of the workspace's running
    /// sessions' process trees (D-104).
    pub(crate) async fn workspace_ports(&self, r: &str) -> RpcResult<Vec<SessionPort>> {
        let ws = self.workspace_get(r).await?;
        let roots: std::collections::HashMap<u32, otter_core::SessionId> = ws
            .sessions
            .iter()
            .filter_map(|s| {
                let e = s.current_execution().filter(|e| e.is_running())?;
                Some((e.pid?, s.id.clone()))
            })
            .collect();
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let ports = crate::metrics::listening_ports().await;
        if ports.iter().all(|p| p.pid.is_none()) {
            return Ok(Vec::new());
        }
        let parents = tokio::task::spawn_blocking(crate::metrics::parent_pids)
            .await
            .unwrap_or_default();
        Ok(ports
            .into_iter()
            .filter_map(|p| {
                Some(SessionPort {
                    session_id: crate::metrics::owner_of(p.pid?, &roots, &parents)?,
                    port: p.port,
                    address: p.address,
                    process: p.process,
                })
            })
            .collect())
    }
}

/// The lines of each new file, within what may be read in all.
fn count_new_files(root: &Path, paths: &[String]) -> Vec<Option<u64>> {
    let mut read = 0u64;
    paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            if i >= COUNTED_FILES || read >= COUNTED_TOTAL {
                return None;
            }
            count_lines(&root.join(path), &mut read)
        })
        .collect()
}

/// A new file's lines, if it's text and not too big to read; `read` adds
/// what was read.
fn count_lines(path: &Path, read: &mut u64) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > COUNTED_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    *read += bytes.len() as u64;
    if bytes.contains(&0) {
        return None;
    }
    let newlines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    Some(newlines + u64::from(bytes.last().is_some_and(|b| *b != b'\n')))
}

/// `git diff --numstat -z`: `added\tdeleted\tpath\0`, or for a rename
/// `added\tdeleted\t\0old\0new\0`; `-` for a binary file's counts.
fn parse_numstat(out: &str) -> Vec<(&str, Option<u64>, Option<u64>)> {
    let mut parts = out.split('\0');
    let mut v = Vec::new();
    while let Some(head) = parts.next() {
        if head.is_empty() {
            continue;
        }
        let mut cols = head.splitn(3, '\t');
        let added = cols.next().and_then(|n| n.parse().ok());
        let deleted = cols.next().and_then(|n| n.parse().ok());
        let path = match cols.next().unwrap_or("") {
            "" => {
                parts.next();
                parts.next().unwrap_or("")
            }
            p => p,
        };
        v.push((path, added, deleted));
    }
    v
}

/// `git diff --name-status -z`: `M\0path\0`, or `R100\0old\0new\0` for a
/// rename (and a copy), with the new path.
fn parse_name_status(out: &str) -> Vec<(ChangeKind, &str)> {
    let mut parts = out.split('\0');
    let mut v = Vec::new();
    while let Some(code) = parts.next() {
        let Some(c) = code.chars().next() else {
            continue;
        };
        let (change, path) = match c {
            'R' | 'C' => {
                parts.next();
                let kind = if c == 'R' {
                    ChangeKind::Renamed
                } else {
                    ChangeKind::Added
                };
                (kind, parts.next())
            }
            'A' => (ChangeKind::Added, parts.next()),
            'D' => (ChangeKind::Deleted, parts.next()),
            _ => (ChangeKind::Modified, parts.next()),
        };
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            v.push((change, p));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_files_are_counted_within_what_may_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![b'x'; COUNTED_BYTES as usize];
        let names: Vec<String> = (0..70).map(|i| format!("f{i}")).collect();
        for n in &names {
            std::fs::write(dir.path().join(n), &big).unwrap();
        }
        std::fs::write(dir.path().join("small"), "a\nb\n").unwrap();
        let counts = count_new_files(dir.path(), &names);
        // 64 MiB in all: the first 64 files are read, the rest are not.
        assert_eq!(counts.iter().filter(|c| c.is_some()).count(), 64);
        assert!(counts[64..].iter().all(Option::is_none));
        assert_eq!(count_new_files(dir.path(), &["small".into()]), [Some(2)]);
    }

    #[test]
    fn reads_numstat_with_renames_and_binaries() {
        let out = "3\t1\tsrc/a.rs\0-\t-\tlogo.png\x000\t0\t\0old.md\0new.md\0";
        assert_eq!(
            parse_numstat(out),
            [
                ("src/a.rs", Some(3), Some(1)),
                ("logo.png", None, None),
                ("new.md", Some(0), Some(0)),
            ]
        );
    }

    #[test]
    fn reads_name_status_with_renames() {
        let out = "M\0src/a.rs\0R087\0old.md\0new.md\0A\0b.txt\0D\0gone\0T\0link\0";
        assert_eq!(
            parse_name_status(out),
            [
                (ChangeKind::Modified, "src/a.rs"),
                (ChangeKind::Renamed, "new.md"),
                (ChangeKind::Added, "b.txt"),
                (ChangeKind::Deleted, "gone"),
                (ChangeKind::Modified, "link"),
            ]
        );
    }

    #[test]
    fn a_long_diff_is_cut_on_a_character() {
        let d = bounded("é".repeat(MAX_DIFF_BYTES));
        assert!(d.truncated);
        assert!(d.text.len() <= MAX_DIFF_BYTES);
    }
}
