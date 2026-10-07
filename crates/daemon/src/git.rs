//! Git repository management (design §13, §14).
//!
//! Each repository is cloned once per host into a bare backing repository,
//! `~/.workd/repos/<repo-id>/base`, and every Git-backed workspace gets its own
//! worktree of it. Worktrees share the object store, so a new workspace costs a
//! fetch and a checkout, not a clone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::env::EnvMap;

pub struct GitManager {
    repos_dir: PathBuf,
    env: EnvMap,
    /// Serializes fetch/worktree operations per backing repository.
    locks: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

/// What a new worktree ended up on.
#[derive(Debug, Clone)]
pub struct Checkout {
    pub branch: String,
    pub base: String,
    pub base_commit: String,
}

impl GitManager {
    pub fn new(repos_dir: PathBuf, env: &EnvMap) -> Self {
        let mut env = env.clone();
        // Never block on a credential prompt: the daemon has no terminal.
        env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        GitManager {
            repos_dir,
            env,
            locks: Default::default(),
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

    /// Make sure the backing repository exists and is up to date.
    pub async fn sync_repo(
        &self,
        repository: &str,
        repo_id: &str,
        progress: impl Fn(&str),
    ) -> Result<PathBuf> {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        let base = self.base_dir(repo_id);
        if !base.join("HEAD").exists() {
            progress("cloning repository");
            std::fs::create_dir_all(base.parent().unwrap())?;
            let tmp = base.with_extension("partial");
            let _ = std::fs::remove_dir_all(&tmp);
            self.git(
                None,
                &["clone", "--bare", "--", repository, &path_str(&tmp)],
            )
            .await
            .context("git clone failed")?;
            // A bare clone maps branches to refs/heads directly; track them as
            // origin/* like a normal clone so `origin/main` resolves.
            self.git(
                Some(&tmp),
                &[
                    "config",
                    "remote.origin.fetch",
                    "+refs/heads/*:refs/remotes/origin/*",
                ],
            )
            .await?;
            std::fs::rename(&tmp, &base)?;
        }
        progress("fetching");
        self.git(Some(&base), &["fetch", "--prune", "origin"])
            .await
            .context("git fetch failed")?;
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
            if !self.ref_exists(&repo, &format!("refs/heads/{b}")).await {
                // Start a local branch tracking the remote one.
                self.git(
                    Some(&repo),
                    &["branch", "--track", b, &format!("origin/{b}")],
                )
                .await?;
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
        self.git(
            Some(&repo),
            &["worktree", "add", "-b", &new_branch, &path_s, &base_commit],
        )
        .await
        .context("git worktree add failed")?;
        Ok(Checkout {
            branch: new_branch,
            base,
            base_commit,
        })
    }

    /// Remove a worktree (discarding uncommitted changes). The branch is kept.
    pub async fn remove_worktree(&self, repo_id: &str, path: &Path) -> Result<()> {
        let lock = self.lock_for(repo_id);
        let _guard = lock.lock().await;
        let repo = self.base_dir(repo_id);
        if path.exists() {
            self.git(
                Some(&repo),
                &["worktree", "remove", "--force", &path_str(path)],
            )
            .await?;
        }
        self.git(Some(&repo), &["worktree", "prune"]).await?;
        Ok(())
    }

    /// Whether the worktree at `path` has uncommitted changes.
    pub async fn is_dirty(&self, path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        let out = self
            .git(
                Some(path),
                &["status", "--porcelain", "--untracked-files=normal"],
            )
            .await?;
        Ok(!out.trim().is_empty())
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
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let out = cmd.output().await.context("running git")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("{}", redact_credentials(stderr.trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

/// Remove `user:password@` from URLs in `s` (tokens embedded in HTTPS URLs).
pub fn redact_credentials(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '/' || c == '\'' || c == '"')
            .unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***@");
                rest = &tail[at + 1..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
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
    fn slugs() {
        assert_eq!(
            slug("Fix GCP renewal validation"),
            "fix-gcp-renewal-validation"
        );
        assert_eq!(slug("  scratch!! "), "scratch");
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
