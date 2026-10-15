//! Environment providers (design §16–§18).
//!
//! Otter activates the environment a repository declares; it does not manage
//! dependencies. V1 knows two kinds:
//!
//! - **None**: the host user's login environment ([`crate::env`]).
//! - **Direnv**: the workspace's `.envrc`, evaluated with `direnv export json`
//!   (which in turn may realize a Nix flake via `use flake`). The result is
//!   applied on top of the login environment, explicitly, before any managed
//!   process starts — no shell hooks involved.
//!
//! direnv is run with otterd's stand-ins (`run/bin`, files.rs) already first
//! on PATH. What it exports remembers the PATH it started from, and a shell
//! hook in a session goes back to that PATH whenever it reloads (a watched
//! file changed) or unloads (`cd` elsewhere): the stand-ins must be in it,
//! or they would drop off PATH mid-session.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use otter_core::EnvironmentKind;
use tokio::process::Command;

use crate::env::{EnvMap, which};

pub struct EnvironmentManager {
    base: EnvMap,
    /// `base` with the stand-ins first on PATH: what direnv runs in.
    with_shim: EnvMap,
    direnv: Option<PathBuf>,
}

impl EnvironmentManager {
    pub fn new(base: &EnvMap, shim: PathBuf) -> Self {
        let mut with_shim = base.clone();
        let path = base.get("PATH").cloned().unwrap_or_default();
        with_shim.insert("PATH".into(), crate::files::shim_first(&shim, &path));
        EnvironmentManager {
            direnv: which("direnv", base),
            base: base.clone(),
            with_shim,
        }
    }

    pub fn base(&self) -> &EnvMap {
        &self.base
    }

    /// Which kind of environment the directory declares.
    pub fn detect(root: &Path) -> EnvironmentKind {
        if root.join(".envrc").is_file() {
            EnvironmentKind::Direnv
        } else {
            EnvironmentKind::None
        }
    }

    /// The complete environment for processes in `root`.
    ///
    /// `authorize` runs `direnv allow` first; Otter does this only for
    /// worktrees it created itself from a repository the user asked for.
    pub async fn resolve(
        &self,
        kind: EnvironmentKind,
        root: &Path,
        authorize: bool,
    ) -> Result<EnvMap> {
        match kind {
            EnvironmentKind::None => Ok(self.base.clone()),
            EnvironmentKind::Direnv => {
                let direnv = self.direnv.as_ref().context(
                    "the workspace has an .envrc but direnv is not installed on this host",
                )?;
                if authorize {
                    self.direnv(direnv, root, &["allow"]).await?;
                }
                let out = self.direnv(direnv, root, &["export", "json"]).await?;
                let mut env = self.with_shim.clone();
                if !out.trim().is_empty() {
                    let diff: std::collections::BTreeMap<String, Option<String>> =
                        serde_json::from_str(&out).context("parsing `direnv export json`")?;
                    for (k, v) in diff {
                        match v {
                            Some(v) => env.insert(k, v),
                            None => env.remove(&k),
                        };
                    }
                }
                Ok(env)
            }
        }
    }

    async fn direnv(&self, direnv: &Path, root: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new(direnv)
            .args(args)
            .current_dir(root)
            .env_clear()
            .envs(&self.with_shim)
            // Quiet "direnv: loading …" chatter; errors still go to stderr.
            .env("DIRENV_LOG_FORMAT", "")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .context("running direnv")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // The interesting part (e.g. a Nix build error) is at the end.
            let tail: Vec<&str> = stderr.trim().lines().rev().take(15).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            // What it printed may hold a secret (an .envrc exports them):
            // never kept as is (D-075).
            bail!(
                "direnv {} failed: {}",
                args.join(" "),
                otter_core::redact::redact(&tail.join("\n"))
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}
