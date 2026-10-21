//! The state of the work in a workspace, read deterministically (D-062):
//! what verification shows the judge instead of the coding agent's word,
//! and a fingerprint that tells whether a fix changed anything.
//!
//! In a Git repository: the change since where the task's own work started
//! (D-167), else since the base the work started from (committed or not,
//! new files included), never touching the index — no
//! `git add -N`, it's the agent's. Files that may hold secrets (`.env`, keys,
//! credentials) count towards the fingerprint but their content is never
//! shown: the report is kept in the task and read by the judge's model.
//! Outside Git: only a fingerprint of the files (path, size, modification
//! time).

use std::hash::Hasher;
use std::path::{Path, PathBuf};

use crate::delivery::run;
use crate::env::{EnvMap, which};

/// How much of the diff goes into the evidence (and the judge's prompt).
const DIFF_CHARS: usize = 8000;
/// New files shown in full (each bounded), and how much of each.
const NEW_FILES: usize = 10;
const NEW_FILE_CHARS: usize = 1500;
const STATUS_LINES: usize = 100;
/// New files whose content counts towards the fingerprint, up to what size
/// each and in all (past that, their size and modification time count).
const HASHED_FILES: usize = 2000;
const HASHED_BYTES: u64 = 1 << 20;
const HASHED_TOTAL: u64 = 64 << 20;
/// Files whose content may be a secret, by name: exactly, by prefix, by
/// suffix. Not exhaustive — the obvious shapes.
const SECRET_NAMES: &[&str] = &[".env", ".netrc", ".npmrc", ".pypirc", "credentials"];
const SECRET_PREFIXES: &[&str] = &[".env.", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
const SECRET_SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore"];
/// Files looked at outside Git.
const TREE_FILES: usize = 20_000;

/// The title of verification's diff evidence (delivery's is "Changes on …").
pub const TITLE: &str = "Working tree";

pub struct WorkState {
    /// Changes whenever the work does.
    pub fingerprint: String,
    /// HEAD, status, the change's stat and a bounded diff; `None` outside Git.
    pub report: Option<String>,
    /// One line: what changed (`2 files changed, …`).
    pub summary: String,
}

/// Read the work's state in `root`. `base`: the branch the work started
/// from, if known (else the change since HEAD is shown).
#[cfg(test)]
pub async fn snapshot(root: &Path, env: &EnvMap, base: Option<&str>) -> WorkState {
    snapshot_since(root, env, base, None).await
}

/// [`snapshot`] of a task's own work (D-167): the change since `own`, the
/// revision the task started from, when HEAD still descends from it; else
/// (not recorded, or the history was rewritten) since the merge base with
/// `base`, as [`snapshot`] reads it.
pub async fn snapshot_since(
    root: &Path,
    env: &EnvMap,
    base: Option<&str>,
    own: Option<&str>,
) -> WorkState {
    if let Some(state) = git_state(root, env, base, own).await {
        return state;
    }
    let dir = root.to_path_buf();
    let fingerprint = tokio::task::spawn_blocking(move || tree_fingerprint(&dir))
        .await
        .unwrap_or_default();
    WorkState {
        fingerprint,
        report: None,
        summary: "not a Git repository".into(),
    }
}

/// Where a snapshot's change starts.
enum Since {
    /// Where the task's own work started (D-167).
    Task(String),
    /// The merge base with the work's base branch, named.
    Base { name: String, rev: String },
    /// HEAD: only what isn't committed.
    Head(String),
}

impl Since {
    fn rev(&self) -> &str {
        match self {
            Since::Task(rev) | Since::Base { rev, .. } | Since::Head(rev) => rev,
        }
    }
}

async fn git_state(
    root: &Path,
    env: &EnvMap,
    base: Option<&str>,
    own: Option<&str>,
) -> Option<WorkState> {
    let git = which("git", env)?;
    // Exit status and stdout; paths limited to the workspace (`-- .`), which
    // may be a directory inside a larger repository.
    let g = |args: Vec<String>| {
        let git = git.clone();
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            match run(&git, &args, root, env, None).await {
                Ok((ok, out, _)) => (ok, out),
                Err(_) => (false, String::new()),
            }
        }
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let (inside, _) = g(s(&["rev-parse", "--is-inside-work-tree"])).await;
    if !inside {
        return None;
    }
    let (has_head, head) = g(s(&["rev-parse", "HEAD"])).await;
    let head = has_head.then(|| head.trim().to_owned());
    // Where the change starts: where the task's own work did, if HEAD
    // still descends from it; else the merge base with the work's base
    // branch; else HEAD (only what isn't committed); else nothing (no
    // commits yet).
    let from = match (&head, own) {
        (Some(_), Some(own)) if descends(&git, root, env, own).await => {
            Some(Since::Task(own.to_owned()))
        }
        (Some(_), _) => merge_base(&git, root, env, base)
            .await
            .map(|(name, rev)| Since::Base { name, rev }),
        (None, _) => None,
    };
    let from = from.or_else(|| head.clone().map(Since::Head));
    let (_, status) = g(s(&["status", "--porcelain=v1", "--", "."])).await;
    let (_, untracked) = g(s(&[
        "ls-files",
        "--others",
        "--exclude-standard",
        "--",
        ".",
    ]))
    .await;
    let untracked: Vec<&str> = untracked.lines().filter(|l| !l.is_empty()).collect();
    // What is shown leaves secret-like files out; the fingerprint doesn't.
    let shown = |mut args: Vec<String>| {
        args.push("--".into());
        args.push(".".into());
        args.extend(secret_pathspecs());
        args
    };
    let (diff, stat, binary) = match from.as_ref().map(Since::rev) {
        Some(rev) => {
            let (_, diff) = g(shown(s(&["diff", rev]))).await;
            let (_, stat) = g(s(&["diff", "--stat", rev, "--", "."])).await;
            let (_, binary) = g(s(&["diff", "--binary", rev, "--", "."])).await;
            (diff, stat, binary)
        }
        None => {
            let (_, diff) = g(shown(s(&["diff", "--cached"]))).await;
            let (_, all) = g(s(&["diff", "--cached", "--binary", "--", "."])).await;
            (diff, String::new(), all)
        }
    };
    // New files, shown as diffs against nothing (bounded).
    let mut new_files = String::new();
    let visible: Vec<&&str> = untracked.iter().filter(|p| !secret_like(p)).collect();
    for path in visible.iter().take(NEW_FILES) {
        let (_, d) = g(s(&["diff", "--no-index", "--", "/dev/null", path])).await;
        new_files.push_str(&bounded(&d, NEW_FILE_CHARS));
    }
    if visible.len() > NEW_FILES {
        new_files.push_str(&format!(
            "… and {} more new files\n",
            visible.len() - NEW_FILES
        ));
    }
    let mut hidden: Vec<&str> = status
        .lines()
        .filter_map(|l| l.get(3..))
        .chain(untracked.iter().copied())
        .filter(|p| secret_like(p))
        .collect();
    hidden.sort_unstable();
    hidden.dedup();

    let mut h = Fnv::new();
    for part in [head.as_deref().unwrap_or(""), &status, &binary] {
        h.write(part.as_bytes());
        h.write(&[0]);
    }
    let dir = root.to_path_buf();
    let paths: Vec<String> = untracked.iter().map(|p| p.to_string()).collect();
    let new_hash = tokio::task::spawn_blocking(move || hash_files(&dir, &paths))
        .await
        .unwrap_or_default();
    h.write(new_hash.as_bytes());

    let stat_line = stat.lines().last().unwrap_or("").trim().to_owned();
    let summary = match (stat_line.is_empty(), untracked.len()) {
        (true, 0) => "no changes".to_owned(),
        (true, n) => format!("{n} new file{}", plural(n)),
        (false, 0) => stat_line,
        (false, n) => format!("{stat_line}; {n} new file{}", plural(n)),
    };
    let mut report = match (&head, &from) {
        (Some(h), Some(Since::Task(rev))) => format!(
            "HEAD {}; changes since this task started (at {})\n",
            short(h),
            short(rev)
        ),
        (Some(h), Some(Since::Base { name, rev })) => format!(
            "HEAD {}; changes since {name} (merge base {})\n",
            short(h),
            short(rev)
        ),
        (Some(h), _) => format!("HEAD {}; changes not yet committed\n", short(h)),
        (None, _) => "No commits yet\n".to_owned(),
    };
    let status_lines: Vec<&str> = status.lines().collect();
    report.push_str("\n$ git status --short\n");
    if status_lines.is_empty() {
        report.push_str("(clean)\n");
    }
    for l in status_lines.iter().take(STATUS_LINES) {
        report.push_str(l);
        report.push('\n');
    }
    if status_lines.len() > STATUS_LINES {
        report.push_str(&format!(
            "… {} more lines\n",
            status_lines.len() - STATUS_LINES
        ));
    }
    if !stat.trim().is_empty() {
        report.push_str(&format!("\n$ git diff --stat\n{}\n", stat.trim_end()));
    }
    let full = format!("{diff}{new_files}");
    if !full.trim().is_empty() {
        report.push('\n');
        report.push_str(&bounded(&full, DIFF_CHARS));
    }
    if !hidden.is_empty() {
        report.push_str(&format!(
            "\nNot shown (may hold secrets): {}\n",
            hidden.join(", ")
        ));
    }
    Some(WorkState {
        fingerprint: format!("{:016x}", h.finish()),
        report: Some(report),
        summary,
    })
}

/// Where the work's changes start: the merge base of HEAD with its base
/// branch, tried as `origin/<base>`, then `<base>`; with the name that
/// worked. `None` without a base, or one this repository doesn't know.
pub(crate) async fn merge_base(
    git: &Path,
    root: &Path,
    env: &EnvMap,
    base: Option<&str>,
) -> Option<(String, String)> {
    for b in base
        .into_iter()
        .flat_map(|b| [format!("origin/{b}"), b.to_owned()])
    {
        if let Ok((true, mb, _)) = run(git, &["merge-base", &b, "HEAD"], root, env, None).await {
            return Some((b, mb.trim().to_owned()));
        }
    }
    None
}

/// Whether HEAD is `rev` or descends from it.
pub(crate) async fn descends(git: &Path, root: &Path, env: &EnvMap, rev: &str) -> bool {
    matches!(
        run(
            git,
            &["merge-base", "--is-ancestor", rev, "HEAD"],
            root,
            env,
            None
        )
        .await,
        Ok((true, _, _))
    )
}

/// A path whose content may be a secret (by its file name).
pub(crate) fn secret_like(path: &str) -> bool {
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    SECRET_NAMES.contains(&name)
        || SECRET_PREFIXES.iter().any(|p| name.starts_with(p))
        || SECRET_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Pathspecs leaving out what [`secret_like`] matches.
pub(crate) fn secret_pathspecs() -> Vec<String> {
    let names = SECRET_NAMES.iter().map(|n| format!("**/{n}"));
    let prefixes = SECRET_PREFIXES.iter().map(|p| format!("**/{p}*"));
    let suffixes = SECRET_SUFFIXES.iter().map(|x| format!("**/*{x}"));
    names
        .chain(prefixes)
        .chain(suffixes)
        .map(|g| format!(":(exclude,glob){g}"))
        .collect()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(10)]
}

/// The start of `text`, at most `max` characters, saying what was left out.
fn bounded(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str(&format!("\n… ({} more characters)\n", n - max));
    out
}

/// New files' content (or, past a size, length and modification time).
fn hash_files(root: &Path, paths: &[String]) -> String {
    let mut h = Fnv::new();
    let mut read = 0u64;
    for p in paths.iter().take(HASHED_FILES) {
        h.write(p.as_bytes());
        let path = root.join(p);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() && m.len() <= HASHED_BYTES && read + m.len() <= HASHED_TOTAL => {
                read += m.len();
                if let Ok(bytes) = std::fs::read(&path) {
                    h.write(&bytes);
                }
            }
            Ok(m) => write_meta(&mut h, &m),
            Err(_) => {}
        }
        h.write(&[0]);
    }
    h.write(paths.len().to_string().as_bytes());
    format!("{:016x}", h.finish())
}

fn write_meta(h: &mut Fnv, m: &std::fs::Metadata) {
    h.write(&m.len().to_le_bytes());
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    h.write(&mtime.to_le_bytes());
}

/// Outside Git: every file's path, size and modification time, leaving out
/// hidden directories and the usual build and dependency directories.
fn tree_fingerprint(root: &Path) -> String {
    const SKIP: &[&str] = &["node_modules", "target", "__pycache__"];
    let mut h = Fnv::new();
    let mut seen = 0usize;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let Ok(m) = e.path().symlink_metadata() else {
                continue;
            };
            if m.is_dir() {
                if !name.starts_with('.') && !SKIP.contains(&name.as_ref()) {
                    stack.push(e.path());
                }
                continue;
            }
            seen += 1;
            if seen > TREE_FILES {
                return format!("{:016x}+", h.finish());
            }
            let rel = e.path();
            let rel = rel.strip_prefix(root).unwrap_or(&rel);
            h.write(rel.to_string_lossy().as_bytes());
            write_meta(&mut h, &m);
        }
    }
    format!("{:016x}", h.finish())
}

/// FNV-1a: small and stable across releases (the fingerprint is persisted).
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn env() -> EnvMap {
        std::env::vars().collect()
    }

    #[tokio::test]
    async fn uncommitted_edits_and_new_files_are_shown_and_fingerprinted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "first"]);
        let clean = snapshot(root, &env(), Some("main")).await;
        assert_eq!(clean.summary, "no changes");
        assert!(clean.report.as_deref().unwrap().contains("(clean)"));

        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("b.txt"), "brand new\n").unwrap();
        let s = snapshot(root, &env(), Some("main")).await;
        let report = s.report.unwrap();
        assert!(report.contains("+two"), "{report}");
        assert!(report.contains("?? b.txt"), "{report}");
        assert!(report.contains("+brand new"), "{report}");
        assert!(s.summary.contains("1 new file"), "{}", s.summary);
        assert_ne!(s.fingerprint, clean.fingerprint);
        // The index is the agent's: nothing was staged.
        let staged = std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(staged.stdout.is_empty());

        // Same state, same fingerprint; any edit (even to a new file) differs.
        assert_eq!(
            snapshot(root, &env(), None).await.fingerprint,
            s.fingerprint
        );
        std::fs::write(root.join("b.txt"), "brand new, edited\n").unwrap();
        assert_ne!(
            snapshot(root, &env(), None).await.fingerprint,
            s.fingerprint
        );

        // Committing shows the change since the base, not an empty diff.
        git(root, &["checkout", "-q", "-b", "work"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "work"]);
        let report = snapshot(root, &env(), Some("main")).await.report.unwrap();
        assert!(report.contains("changes since main"), "{report}");
        assert!(report.contains("+two"), "{report}");
    }

    /// B09's shape (D-167): the branch carries an earlier task's commit that
    /// the base, as last fetched, doesn't have. A task started after it is
    /// diffed from where it started, not from the merge base, so the earlier
    /// commit isn't its change. Without that start, or with a history that
    /// no longer descends from it, the merge base it is.
    #[tokio::test]
    async fn a_task_is_diffed_from_where_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(
            root.join("money.py"),
            "def parse(): pass\ndef format(): pass\n",
        )
        .unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "first"]);
        git(root, &["checkout", "-q", "-b", "otterd/ws"]);
        // The earlier task's work, committed on the shared branch.
        std::fs::write(
            root.join("money.py"),
            "def parse(): return EARLIER\ndef format(): pass\n",
        )
        .unwrap();
        git(root, &["commit", "-q", "-am", "the earlier task"]);
        let start = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        // This task's own change.
        std::fs::write(
            root.join("money.py"),
            "def parse(): return EARLIER\ndef format(): return MINE\n",
        )
        .unwrap();

        let own = snapshot_since(root, &env(), Some("main"), Some(&start)).await;
        let report = own.report.unwrap();
        assert!(report.contains("+def format(): return MINE"), "{report}");
        assert!(!report.contains("+def parse"), "{report}");
        assert!(
            report.starts_with(&format!(
                "HEAD {}; changes since this task started (at {})",
                &start[..10],
                &start[..10]
            )),
            "{report}"
        );
        // No start recorded: the merge base with the base branch, the earlier
        // task's commit included.
        let report = snapshot_since(root, &env(), Some("main"), None)
            .await
            .report
            .unwrap();
        assert!(report.contains("changes since main"), "{report}");
        assert!(report.contains("+def parse(): return EARLIER"), "{report}");
        // A start HEAD doesn't descend from (a rewritten history): the same.
        git(root, &["commit", "-q", "--amend", "-am", "rewritten"]);
        let report = snapshot_since(root, &env(), Some("main"), Some(&start))
            .await
            .report
            .unwrap();
        assert!(report.contains("changes since main"), "{report}");
    }

    /// A secret's content never reaches the report, but changing it is
    /// still a change.
    #[tokio::test]
    async fn secrets_count_but_are_never_shown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("config.env"), "PORT=1\n").unwrap();
        std::fs::create_dir_all(root.join("deploy")).unwrap();
        std::fs::write(root.join("deploy/.env"), "TOKEN=old-secret\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "first"]);
        std::fs::write(root.join("deploy/.env"), "TOKEN=new-secret\n").unwrap();
        std::fs::write(root.join(".env.local"), "KEY=very-secret\n").unwrap();
        std::fs::write(root.join("server.pem"), "-----BEGIN-----\n").unwrap();
        std::fs::write(root.join("app.txt"), "visible\n").unwrap();
        let s = snapshot(root, &env(), Some("main")).await;
        let report = s.report.unwrap();
        for secret in ["old-secret", "new-secret", "very-secret", "BEGIN"] {
            assert!(!report.contains(secret), "{secret} in {report}");
        }
        assert!(report.contains("+visible"), "{report}");
        assert!(
            report.contains("Not shown (may hold secrets): .env.local, deploy/.env, server.pem"),
            "{report}"
        );
        std::fs::write(root.join(".env.local"), "KEY=rotated\n").unwrap();
        assert_ne!(
            snapshot(root, &env(), None).await.fingerprint,
            s.fingerprint
        );
        assert!(!secret_like("config.env"));
        assert!(secret_like("a/b/id_ed25519.pub"));
    }

    #[tokio::test]
    async fn outside_git_only_the_files_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        let s = snapshot(root, &env(), None).await;
        assert!(s.report.is_none());
        // Build output doesn't count; the work does.
        std::fs::write(root.join("target/out"), "x").unwrap();
        assert_eq!(
            snapshot(root, &env(), None).await.fingerprint,
            s.fingerprint
        );
        std::fs::write(root.join("a.txt"), "one, two").unwrap();
        assert_ne!(
            snapshot(root, &env(), None).await.fingerprint,
            s.fingerprint
        );
    }

    #[test]
    fn long_text_is_cut_with_a_note() {
        assert_eq!(bounded("abc", 5), "abc");
        assert!(bounded(&"x".repeat(20), 5).contains("15 more characters"));
    }
}
