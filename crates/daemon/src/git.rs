//! Git repository management (design §13, §14).
//!
//! Each repository is cloned once per host into a bare backing repository,
//! `~/.otter/repos/<repo-id>/base`, and every Git-backed workspace gets its own
//! worktree of it. Worktrees share the object store, so a new workspace costs a
//! fetch and a checkout, not a clone.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::env::EnvMap;

/// Set in a backing repository whose refs/heads holds only the workspaces'
/// branches, none copied from the remote by a clone (D-132).
const OWN_BRANCHES_ONLY: &str = "otter.ownBranchesOnly";

pub struct GitManager {
    repos_dir: PathBuf,
    env: EnvMap,
    /// Otter's git identity and key, which every repository includes (D-133).
    account: Arc<crate::git_account::GitAccount>,
    /// Serializes fetch/worktree operations per backing repository.
    locks: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

/// What is at a workspace's worktree path, as git sees it (D-135).
#[derive(Debug)]
pub enum Found {
    /// No `.git` there: nothing, an empty directory, or the registration of
    /// a worktree whose directory is gone (and why it's locked, if it is).
    Nothing { locked: Option<String> },
    /// A worktree of this repository: its branch (none when detached), and
    /// whether `git worktree add` never finished it — locked
    /// "initializing", as git leaves it until its checkout is done.
    Worktree {
        branch: Option<String>,
        cut_short: bool,
    },
    /// Files, and no worktree of this repository.
    Other,
}

/// What a new worktree ended up on.
#[derive(Debug, Clone)]
pub struct Checkout {
    pub branch: String,
    pub base: String,
    pub base_commit: String,
}

impl GitManager {
    pub fn new(
        repos_dir: PathBuf,
        env: &EnvMap,
        account: Arc<crate::git_account::GitAccount>,
    ) -> Self {
        let mut env = env.clone();
        // Never block on a credential prompt: the daemon has no terminal.
        env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        GitManager {
            repos_dir,
            env,
            account,
            locks: Default::default(),
        }
    }

    /// Make `repo` include Otter's git settings (D-133), once.
    async fn include(&self, repo: &Path) -> Result<()> {
        let file = path_str(&self.account.config_file());
        let has = self
            .git(Some(repo), &["config", "--get-all", "include.path"])
            .await
            .unwrap_or_default();
        if !has.lines().any(|l| l.trim() == file) {
            self.git(Some(repo), &["config", "--add", "include.path", &file])
                .await?;
        }
        Ok(())
    }

    /// Every repository Otter manages includes its git settings.
    pub async fn include_everywhere(&self) {
        let Ok(entries) = std::fs::read_dir(&self.repos_dir) else {
            return;
        };
        for e in entries.flatten() {
            let id = e.file_name().to_string_lossy().into_owned();
            let base = self.base_dir(&id);
            if !base.join("HEAD").exists() {
                continue;
            }
            let lock = self.lock_for(&id);
            let _guard = lock.lock().await;
            if let Err(err) = self.include(&base).await {
                tracing::warn!(repo = %id, "including Otter's git settings: {err:#}");
            }
        }
    }

    fn lock_for(&self, repo_id: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(repo_id.to_owned())
            .or_default()
            .clone()
    }

    pub fn base_dir(&self, repo_id: &str) -> PathBuf {
        self.repos_dir.join(repo_id).join("base")
    }

    /// Whether the backing repository is there, so preparing only fetches.
    pub fn cloned(&self, repo_id: &str) -> bool {
        self.base_dir(repo_id).join("HEAD").exists()
    }

    /// Make sure the backing repository exists and is up to date.
    pub async fn sync_repo(&self, repository: &str, repo_id: &str) -> Result<PathBuf> {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        let base = self.base_dir(repo_id);
        if !base.join("HEAD").exists() {
            std::fs::create_dir_all(base.parent().unwrap())?;
            let tmp = base.with_extension("partial");
            let _ = std::fs::remove_dir_all(&tmp);
            // Not `git clone --bare`: it copies every branch the remote has
            // into refs/heads, where they stay as they were that day —
            // checked out stale, and on no remote once deleted there
            // (D-132). The remote's branches are origin/* (`remote add` sets
            // that up); refs/heads holds only the workspaces' own.
            self.git(None, &["init", "--quiet", "--bare", "--", &path_str(&tmp)])
                .await?;
            // Otter's git settings from the start (D-133): its key, once it
            // works, for the first fetch too.
            self.include(&tmp).await?;
            // As `clone` would, a local path made absolute: fetches run in
            // the copy.
            let url = match Path::new(repository) {
                p if p.is_relative() && p.exists() => path_str(&std::path::absolute(p)?),
                _ => repository.to_owned(),
            };
            self.git(Some(&tmp), &["remote", "add", "--", "origin", &url])
                .await?;
            self.git(Some(&tmp), &["config", OWN_BRANCHES_ONLY, "true"])
                .await?;
            self.git(Some(&tmp), &["fetch", "--tags", "origin"])
                .await
                .context("git clone failed")?;
            std::fs::rename(&tmp, &base)?;
        }
        self.include(&base).await?;
        self.git(Some(&base), &["fetch", "--prune", "origin"])
            .await
            .context("git fetch failed")?;
        self.drop_cloned_branches(&base).await;
        if self
            .git(Some(&base), &["remote", "set-head", "origin", "--auto"])
            .await
            .is_err()
        {
            tracing::debug!(repo_id, "could not determine origin's default branch");
        }
        Ok(base)
    }

    /// Create a worktree at `path`.
    ///
    /// - `branch` given and it exists (locally or as `origin/<branch>`): check
    ///   it out.
    /// - Otherwise create a new branch (`branch`, or `default_branch`) from
    ///   `base` (default: origin's default branch).
    pub async fn add_worktree(
        &self,
        repo_id: &str,
        path: &Path,
        branch: Option<&str>,
        base: Option<&str>,
        default_branch: &str,
    ) -> Result<Checkout> {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        let repo = self.base_dir(repo_id);
        let path_s = path_str(path);

        if let Some(b) = branch
            && (self.ref_exists(&repo, &format!("refs/heads/{b}")).await
                || self
                    .ref_exists(&repo, &format!("refs/remotes/origin/{b}"))
                    .await)
        {
            let (local, remote) = (format!("refs/heads/{b}"), format!("origin/{b}"));
            if !self.ref_exists(&repo, &local).await {
                // Start a local branch tracking the remote one.
                self.git(Some(&repo), &["branch", "--track", b, &remote])
                    .await?;
            } else if self
                .ref_exists(&repo, &format!("refs/remotes/{remote}"))
                .await
                && self
                    .git(
                        Some(&repo),
                        &["merge-base", "--is-ancestor", &local, &remote],
                    )
                    .await
                    .is_ok()
            {
                // One left here behind the remote's: catch it up, which loses
                // nothing (D-132). Checked out elsewhere, git refuses, and
                // `worktree add` below says why.
                let _ = self
                    .git(Some(&repo), &["branch", "--force", "--track", b, &remote])
                    .await;
            }
            self.git(Some(&repo), &["worktree", "add", &path_s, b])
                .await
                .context("git worktree add failed")?;
            let commit = self.rev_parse(&repo, b).await?;
            return Ok(Checkout {
                branch: b.to_owned(),
                base: b.to_owned(),
                base_commit: commit,
            });
        }

        let base = match base {
            Some(b) => b.to_owned(),
            None => self.default_base(&repo).await?,
        };
        let base_commit = self
            .rev_parse(&repo, &format!("{base}^{{commit}}"))
            .await
            .with_context(|| format!("unknown base revision `{base}`"))?;
        let new_branch = match branch {
            Some(b) => b.to_owned(),
            None => self.unused_branch_name(&repo, default_branch).await,
        };
        if let Err(e) = self
            .git(
                Some(&repo),
                &["worktree", "add", "-b", &new_branch, &path_s, &base_commit],
            )
            .await
        {
            // git makes the branch first and keeps it when the worktree
            // fails, so each retry would take the next name (D-135). It is
            // this call's: a name in use is never picked, and none appears
            // meanwhile under this lock.
            if self
                .ref_exists(&repo, &format!("refs/heads/{new_branch}"))
                .await
            {
                let _ = self
                    .git(Some(&repo), &["branch", "-D", "--", &new_branch])
                    .await;
            }
            return Err(e.context("git worktree add failed"));
        }
        Ok(Checkout {
            branch: new_branch,
            base,
            base_commit,
        })
    }

    /// Remove the worktree at `path` (discarding uncommitted changes) and
    /// whatever else is there. The branch it had checked out is kept, and
    /// returned. Locked ones go too — a checkout cut short leaves its
    /// worktree locked "initializing" — and so does the registration of one
    /// whose directory went missing (D-135). `path` is Otter's own, under
    /// `~/.otter/workspaces`.
    pub async fn remove_worktree(&self, repo_id: &str, path: &Path) -> Result<Option<String>> {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        let repo = self.base_dir(repo_id);
        let branch = match registration(&repo, path) {
            Some(r) => {
                // Twice: locked ones too.
                self.git(
                    Some(&repo),
                    &["worktree", "remove", "--force", "--force", &path_str(path)],
                )
                .await?;
                r.branch
            }
            None => None,
        };
        self.git(Some(&repo), &["worktree", "prune"]).await?;
        // One git doesn't know: a checkout cut short before it wrote `.git`.
        match tokio::fs::remove_dir_all(path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(branch),
        }
    }

    /// What is at `path`, a workspace's worktree path (D-135).
    pub fn found(&self, repo_id: &str, path: &Path) -> Found {
        let registered = registration(&self.base_dir(repo_id), path);
        if !path.join(".git").exists() {
            let empty = std::fs::read_dir(path).map_or(true, |mut d| d.next().is_none());
            return match empty {
                true => Found::Nothing {
                    locked: registered.and_then(|r| r.locked),
                },
                false => Found::Other,
            };
        }
        match registered {
            Some(r) => Found::Worktree {
                cut_short: r.locked.as_deref() == Some("initializing"),
                branch: r.branch,
            },
            None => Found::Other,
        }
    }

    /// Unlock the worktree at `path`.
    pub async fn unlock(&self, repo_id: &str, path: &Path) -> Result<()> {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        self.git(
            Some(&self.base_dir(repo_id)),
            &["worktree", "unlock", &path_str(path)],
        )
        .await?;
        Ok(())
    }

    /// What a worktree found on `branch` came from, as `add_worktree` would
    /// have said: `base` (default: origin's default branch), and where the
    /// branch left it. Best effort (D-135).
    pub async fn base_of(
        &self,
        repo_id: &str,
        branch: &str,
        base: Option<&str>,
    ) -> (Option<String>, Option<String>) {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        let repo = self.base_dir(repo_id);
        let base = match base {
            Some(b) => Some(b.to_owned()),
            None => self.default_base(&repo).await.ok(),
        };
        let commit = match &base {
            Some(b) => self
                .git(
                    Some(&repo),
                    &["merge-base", &format!("refs/heads/{branch}"), b],
                )
                .await
                .ok()
                .map(|c| c.trim().to_owned()),
            None => None,
        };
        (base, commit)
    }

    /// Delete `branch` if nothing is on it: no remote branch has its name,
    /// and it holds no commit a remote lacks. What `git worktree add -b`
    /// made for a checkout that was cut short, so that making it again takes
    /// the same name (D-135).
    pub async fn drop_empty_branch(&self, repo_id: &str, branch: &str) {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        let repo = self.base_dir(repo_id);
        if branch.is_empty()
            || !self
                .ref_exists(&repo, &format!("refs/heads/{branch}"))
                .await
            || self
                .ref_exists(&repo, &format!("refs/remotes/origin/{branch}"))
                .await
            || !matches!(self.unpushed_in(&repo, branch).await, Ok(0))
        {
            return;
        }
        if let Err(e) = self.git(Some(&repo), &["branch", "-D", "--", branch]).await {
            tracing::warn!(repo_id, branch, "keeping the branch: {e:#}");
        }
    }

    /// Commits on `branch` that no remote-tracking branch has: what deleting
    /// the branch would lose (D-108). 0 when there's no such branch, or when
    /// its work landed on the default branch as other commits (D-132).
    pub async fn unpushed(&self, repo_id: &str, branch: &str) -> Result<u32> {
        self.unpushed_in(&self.base_dir(repo_id), branch).await
    }

    async fn unpushed_in(&self, repo: &Path, branch: &str) -> Result<u32> {
        let head = format!("refs/heads/{branch}");
        if branch.is_empty() || !self.ref_exists(repo, &head).await {
            return Ok(0);
        }
        let out = self
            .git(Some(repo), &["rev-list", &head, "--not", "--remotes"])
            .await?;
        let commits: HashSet<&str> = out.lines().collect();
        if commits.is_empty() || self.landed(repo, &head, &commits).await {
            return Ok(0);
        }
        Ok(commits.len() as u32)
    }

    /// Whether `commits` (`head`'s, on no remote) are on the default branch
    /// under other commits: each one re-applied (a rebase merge), or the
    /// whole branch as one (a squash merge) — what a merged pull request
    /// leaves once its branch is deleted. Compared by patch id, so work
    /// added since never counts.
    async fn landed(&self, repo: &Path, head: &str, commits: &HashSet<&str>) -> bool {
        let Ok(upstream) = self.default_base(repo).await else {
            return false;
        };
        // `-` marks a commit with an equivalent upstream. Merge commits
        // aren't listed: a branch with one of its own isn't landed this way.
        let Ok(cherry) = self.git(Some(repo), &["cherry", &upstream, head]).await else {
            return false;
        };
        let equivalent: HashSet<&str> = cherry
            .lines()
            .filter_map(|l| l.strip_prefix("- "))
            .collect();
        if commits.is_subset(&equivalent) {
            return true;
        }
        let squashed = async {
            let base = self
                .git(Some(repo), &["merge-base", &upstream, head])
                .await?;
            let tree = self.rev_parse(repo, &format!("{head}^{{tree}}")).await?;
            // No change at all: nothing a squash could have carried, and an
            // empty patch would match any empty commit upstream.
            if tree
                == self
                    .rev_parse(repo, &format!("{}^{{tree}}", base.trim()))
                    .await?
            {
                return Ok(String::new());
            }
            // The whole branch as one commit on its merge base: an object
            // only, on no ref; gc takes it.
            let commit = self
                .git(
                    Some(repo),
                    &[
                        "-c",
                        "user.name=otterd",
                        "-c",
                        "user.email=otterd@localhost",
                        "commit-tree",
                        "--no-gpg-sign",
                        "-p",
                        base.trim(),
                        "-m",
                        "squashed",
                        &tree,
                    ],
                )
                .await?;
            self.git(Some(repo), &["cherry", &upstream, commit.trim()])
                .await
        };
        matches!(squashed.await, Ok(out) if out.starts_with('-'))
    }

    /// Delete a workspace's branch once its worktree is gone (D-108).
    /// `false`: kept — another worktree has it checked out, or git said no.
    pub async fn delete_branch(&self, repo_id: &str, branch: &str) -> bool {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        let repo = self.base_dir(repo_id);
        if branch.is_empty()
            || !self
                .ref_exists(&repo, &format!("refs/heads/{branch}"))
                .await
        {
            return true;
        }
        match self.git(Some(&repo), &["branch", "-D", "--", branch]).await {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(repo_id, branch, "keeping the branch: {e:#}");
                false
            }
        }
    }

    /// Remove the backing repository once no workspace uses it (D-108) —
    /// unless a branch in it still holds commits on no remote (one kept by
    /// a delete from before, say): then it stays, with that work. `in_use`
    /// is awaited with this repository's lock held, so a workspace being
    /// made on it is either counted or fetches only once the copy is gone
    /// (D-135).
    pub async fn remove_repo(
        &self,
        repo_id: &str,
        in_use: impl Future<Output = bool>,
    ) -> Result<bool> {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        if in_use.await {
            return Ok(false);
        }
        let repo = self.base_dir(repo_id);
        if repo.join("HEAD").exists() {
            self.drop_cloned_branches(&repo).await;
            let branches = self
                .git(
                    Some(&repo),
                    &["for-each-ref", "--format=%(refname:lstrip=2)", "refs/heads"],
                )
                .await?;
            for branch in branches.lines() {
                if self.unpushed_in(&repo, branch).await? > 0 {
                    tracing::info!(
                        repo_id,
                        branch,
                        "keeping the repository: a branch holds unpushed commits"
                    );
                    return Ok(false);
                }
            }
        }
        let dir = self.repos_dir.join(repo_id);
        match tokio::fs::remove_dir_all(&dir).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", dir.display()))
            }
            _ => Ok(true),
        }
    }

    /// A repository cloned before D-132 holds a copy of each branch the
    /// remote had that day. Drop those once: refs the clone packed, checked
    /// out nowhere, with no reflog. The reflog is what keeps work: a branch
    /// committed on here has one even once a gc has packed it (the loose
    /// ref check only spares branches a workspace made and never used), so
    /// nothing goes while git is set not to write them. Each one dropped is
    /// logged with its commit. Best effort; a failure tries again next time.
    async fn drop_cloned_branches(&self, repo: &Path) {
        if self
            .git(Some(repo), &["config", "--get", OWN_BRANCHES_ONLY])
            .await
            .is_ok()
        {
            return;
        }
        // `bool-or-str`: every spelling of false reads `false`; `always` stays.
        if let Ok(v) = self
            .git(
                Some(repo),
                &[
                    "config",
                    "--type=bool-or-str",
                    "--get",
                    "core.logAllRefUpdates",
                ],
            )
            .await
            && v.trim() == "false"
        {
            tracing::info!(repo = %repo.display(), "keeping cloned branches: reflogs are off");
            return;
        }
        let checked_out = self
            .git(Some(repo), &["worktree", "list", "--porcelain"])
            .await
            .unwrap_or_default();
        let checked_out: Vec<&str> = checked_out
            .lines()
            .filter_map(|l| l.strip_prefix("branch "))
            .collect();
        let packed = std::fs::read_to_string(repo.join("packed-refs")).unwrap_or_default();
        let cloned: Vec<(&str, &str)> = packed
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter_map(|(commit, name)| Some((commit, name.strip_prefix("refs/heads/")?)))
            .filter(|(_, branch)| {
                let name = format!("refs/heads/{branch}");
                !checked_out.contains(&name.as_str())
                    && !repo.join(&name).exists()
                    && !repo.join("logs").join(&name).exists()
            })
            .collect();
        if !cloned.is_empty() {
            for (commit, branch) in &cloned {
                tracing::info!(repo = %repo.display(), branch, commit, "dropping a cloned branch");
            }
            let mut args = vec!["branch", "--delete", "--force", "--"];
            args.extend(cloned.iter().map(|(_, branch)| *branch));
            if let Err(e) = self.git(Some(repo), &args).await {
                tracing::warn!(repo = %repo.display(), "dropping cloned branches: {e:#}");
                return;
            }
        }
        if let Err(e) = self
            .git(Some(repo), &["config", OWN_BRANCHES_ONLY, "true"])
            .await
        {
            tracing::warn!(repo = %repo.display(), "{e:#}");
        }
    }

    /// Whether the worktree at `path` has uncommitted changes: `Some(true)`
    /// when they are only files Git doesn't track (D-189).
    pub async fn uncommitted(&self, path: &Path) -> Result<Option<bool>> {
        if !path.exists() {
            return Ok(None);
        }
        let out = self
            .git(
                Some(path),
                &["status", "--porcelain", "--untracked-files=normal"],
            )
            .await?;
        let mut lines = out.lines().filter(|l| !l.trim().is_empty()).peekable();
        if lines.peek().is_none() {
            return Ok(None);
        }
        Ok(Some(lines.all(|l| l.starts_with("??"))))
    }

    async fn default_base(&self, repo: &Path) -> Result<String> {
        if let Ok(head) = self
            .git(
                Some(repo),
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )
            .await
        {
            return Ok(head.trim().to_owned());
        }
        for candidate in ["origin/main", "origin/master"] {
            if self
                .ref_exists(repo, &format!("refs/remotes/{candidate}"))
                .await
            {
                return Ok(candidate.to_owned());
            }
        }
        bail!("could not determine the default branch; pass a base revision")
    }

    async fn unused_branch_name(&self, repo: &Path, wanted: &str) -> String {
        let mut name = wanted.to_owned();
        let mut n = 2;
        while self.ref_exists(repo, &format!("refs/heads/{name}")).await {
            name = format!("{wanted}-{n}");
            n += 1;
        }
        name
    }

    async fn ref_exists(&self, repo: &Path, refname: &str) -> bool {
        self.git(Some(repo), &["show-ref", "--verify", "--quiet", refname])
            .await
            .is_ok()
    }

    async fn rev_parse(&self, repo: &Path, rev: &str) -> Result<String> {
        Ok(self
            .git(Some(repo), &["rev-parse", "--verify", "--quiet", rev])
            .await?
            .trim()
            .to_owned())
    }

    async fn git(&self, dir: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("git");
        if let Some(dir) = dir {
            cmd.arg("-C").arg(dir);
        }
        cmd.args(args)
            .env_clear()
            .envs(&self.env)
            // Reading takes no lock on a worktree's index (as in delivery).
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, ended whole when this is dropped (a
            // preparation cancelled, otterd stopping): `worktree add` checks
            // out in a child that would go on writing into the tree (D-135).
            .process_group(0)
            .kill_on_drop(true);
        let child = cmd.spawn().context("running git")?;
        let mut group = Group(child.id());
        let out = child.wait_with_output().await.context("running git")?;
        group.0 = None;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("{}", redact_credentials(stderr.trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// The process group a running git leads, killed if it is dropped before
/// git is done (D-135).
struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(leader) = self.0 {
            // SAFETY: a plain signal to the group this git leads. Not yet
            // waited for, or waited for while its children still hold its
            // output open: either way the group's id is still its own.
            unsafe { libc::killpg(leader as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// A worktree's registration in its repository, `worktrees/<id>/`, read
/// from the files git-worktree(1) documents there.
struct Registration {
    /// The branch it has checked out; none when detached.
    branch: Option<String>,
    /// Why it's locked, when it is.
    locked: Option<String>,
}

/// The registration in `repo` of a worktree at `path`, if there is one.
fn registration(repo: &Path, path: &Path) -> Option<Registration> {
    let want = real(path);
    std::fs::read_dir(repo.join("worktrees"))
        .ok()?
        .flatten()
        .find_map(|e| {
            let admin = e.path();
            // `<worktree>/.git`; relative to here when git writes relative paths.
            let gitdir = std::fs::read_to_string(admin.join("gitdir")).ok()?;
            let at = admin.join(gitdir.trim());
            (real(at.parent()?) == want).then(|| Registration {
                branch: std::fs::read_to_string(admin.join("HEAD"))
                    .ok()
                    .and_then(|h| h.trim().strip_prefix("ref: refs/heads/").map(str::to_owned)),
                locked: std::fs::read_to_string(admin.join("locked"))
                    .ok()
                    .map(|r| r.trim().to_owned()),
            })
        })
}

/// `p` with symbolic links resolved, as git records a worktree's path; for
/// one that no longer exists, its nearest existing ancestor's.
fn real(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => real(parent).join(name),
        _ => p.to_owned(),
    }
}

/// Stable, filesystem-safe id for a repository URL: `<name>-<hash>`.
pub fn repo_id(repository: &str) -> String {
    let normalized = repository
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let name: String = normalized
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("repo")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = if name.is_empty() || name.starts_with('.') {
        "repo".to_owned()
    } else {
        name
    };
    format!("{name}-{:08x}", fnv1a(normalized.as_bytes()) as u32)
}

/// The readable part of a repository's name: `suger-api` for
/// `git@github.com:sugerio/suger-api.git` (`repo_id` without its hash).
pub fn repo_name(repository: &str) -> String {
    let id = repo_id(repository);
    match id.rsplit_once('-') {
        Some((name, _)) => name.to_owned(),
        None => id,
    }
}

/// A branch-name-friendly version of a workspace name.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(48).collect()
}

/// What Git said, without credentials in it (D-075): tokens in URLs, and
/// the other shapes `otter_core::redact` knows.
pub fn redact_credentials(s: &str) -> String {
    otter_core::redact::redact(s)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ids_are_stable_and_readable() {
        let a = repo_id("git@github.com:sugerio/suger-api.git");
        let b = repo_id("git@github.com:sugerio/suger-api");
        assert_eq!(a, b);
        assert!(a.starts_with("suger-api-"), "{a}");
        assert_ne!(a, repo_id("git@github.com:other/suger-api.git"));
        assert!(repo_id("/srv/git/thing/").starts_with("thing-"));
    }

    #[test]
    fn repo_names_are_the_readable_part() {
        assert_eq!(
            repo_name("git@github.com:sugerio/suger-api.git"),
            "suger-api"
        );
        assert_eq!(repo_name("https://github.com/acme/billing/"), "billing");
    }

    #[test]
    fn slugs() {
        assert_eq!(
            slug("Fix GCP renewal validation"),
            "fix-gcp-renewal-validation"
        );
        assert_eq!(slug("  scratch!! "), "scratch");
    }

    /// A `git worktree add -b` that fails takes back the branch git made
    /// for it, so trying again takes the same name, not `-2` (D-135).
    #[tokio::test]
    async fn a_failed_worktree_leaves_no_branch_behind() {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&origin)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(origin.join("a.txt"), "a\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first"]);
        let account = Arc::new(crate::git_account::GitAccount::load(
            &dir.path().join("state"),
        ));
        let env = std::env::vars().collect();
        let git = GitManager::new(dir.path().join("repos"), &env, account);
        git.sync_repo(&path_str(&origin), "r").await.unwrap();
        let repo = git.base_dir("r");

        // Something in the way: git makes the branch, then fails.
        let path = dir.path().join("ws/repo");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("in-the-way"), "").unwrap();
        let made = git.add_worktree("r", &path, None, None, "otterd/x").await;
        assert!(made.is_err());
        assert!(!git.ref_exists(&repo, "refs/heads/otterd/x").await);
        // Asked for by name and not there: the same.
        let made = git
            .add_worktree("r", &path, Some("topic"), None, "otterd/x")
            .await;
        assert!(made.is_err());
        assert!(!git.ref_exists(&repo, "refs/heads/topic").await);

        std::fs::remove_dir_all(&path).unwrap();
        let checkout = git
            .add_worktree("r", &path, None, None, "otterd/x")
            .await
            .unwrap();
        assert_eq!(checkout.branch, "otterd/x");
    }

    #[test]
    fn redacts_tokens() {
        assert_eq!(
            redact_credentials("fatal: could not read from https://x:ghp_secret@github.com/a/b"),
            "fatal: could not read from https://***@github.com/a/b"
        );
        assert_eq!(
            redact_credentials("git@github.com:a/b"),
            "git@github.com:a/b"
        );
    }
}
