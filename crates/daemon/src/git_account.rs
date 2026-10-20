//! Otter's own git on this host (D-132): who commits in the repositories it
//! manages, the SSH key they push with, and the GitHub account its own `gh`
//! uses, all apart from the host's own git, ssh and gh.
//!
//! ```text
//! state/git/account.json      { name, email, key_account, fingerprint, github }
//! state/git/id_ed25519(.pub)  made here; the private half never leaves (0600)
//! state/git/known_hosts       GitHub's host keys, as Otter's ssh saw them
//! state/git/gh/               GH_CONFIG_DIR of Otter's gh; its token in hosts.yml
//! state/git/gitconfig         what every repository Otter manages includes
//! ```
//!
//! Each repository under `~/.otter/repos` includes `gitconfig`
//! (`include.path`), which its worktrees share: the identity as
//! `user.name`/`user.email`, and, once the key has signed in to GitHub,
//! `core.sshCommand` with that key alone and `pushInsteadOf` so a push to an
//! https GitHub remote goes over it too. Fetches stay as they were until the
//! key works. Nothing outside those repositories changes. Otter's own gh
//! calls run with its config directory, never a token from the environment;
//! the agent and the terminals don't get it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use otter_protocol::host::{CheckLine, GitHubLogin, GitIdentity, GitView, SshKey};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::env::{EnvMap, which};

/// What gh would take over its config directory, or refuse to sign in with.
const GH_OVERRIDES: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
];
const GITHUB_SSH: &str = "git@github.com";
/// GitHub's published host key (SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU):
/// the first connection is checked, not trusted.
const GITHUB_HOST_KEY: &str =
    "github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
/// The same key for GitHub's port-443 address (behind firewalls).
const GITHUB_443_HOST_KEY: &str = "[ssh.github.com]:443 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
/// GitHub's device codes last 15 minutes.
const LOGIN_WAIT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct Account {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    /// The GitHub account the key signed in as: set, the repositories use it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    /// Who Otter's gh is signed in as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github: Option<String>,
}

pub struct GitAccount {
    dir: PathBuf,
    account: RwLock<Account>,
    login: Mutex<Option<GitHubLogin>>,
    /// The browser sign-in's gh, while it waits.
    waiting: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl GitAccount {
    pub fn load(state_dir: &Path) -> GitAccount {
        let dir = state_dir.join("git");
        let account = crate::settings::read_json(&dir.join("account.json")).unwrap_or_else(|e| {
            tracing::warn!("Otter's git account: {e:#}");
            Account::default()
        });
        let me = GitAccount {
            dir,
            account: RwLock::new(account),
            login: Mutex::new(None),
            waiting: Mutex::new(None),
        };
        if let Err(e) = me.write_config(&me.account.read().unwrap()) {
            tracing::warn!("writing Otter's gitconfig: {e:#}");
        }
        me
    }

    fn key(&self) -> PathBuf {
        self.dir.join("id_ed25519")
    }

    fn public_key(&self) -> PathBuf {
        self.dir.join("id_ed25519.pub")
    }

    fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }

    fn gh_dir(&self) -> PathBuf {
        self.dir.join("gh")
    }

    /// What every repository Otter manages includes.
    pub fn config_file(&self) -> PathBuf {
        self.dir.join("gitconfig")
    }

    fn signed_in(&self) -> bool {
        self.gh_dir().join("hosts.yml").exists()
    }

    pub fn view(&self) -> GitView {
        let a = self.account.read().unwrap().clone();
        let public = std::fs::read_to_string(self.public_key())
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        GitView {
            name: a.name,
            email: a.email,
            ssh_key: public.map(|public| SshKey {
                public,
                fingerprint: a.fingerprint,
            }),
            key_account: a.key_account.filter(|_| self.key().exists()),
            github: a.github.filter(|_| self.signed_in()),
            login: self.login.lock().unwrap().clone(),
        }
    }

    /// Change the account; written (with `gitconfig`) only if it changed.
    fn update(&self, f: impl FnOnce(&mut Account)) -> Result<bool> {
        let mut a = self.account.write().unwrap();
        let mut next = a.clone();
        f(&mut next);
        if next == *a {
            return Ok(false);
        }
        private_dir(&self.dir)?;
        crate::settings::write_private(
            &self.dir.join("account.json"),
            &serde_json::to_vec_pretty(&next)?,
        )?;
        self.write_config(&next)?;
        *a = next;
        Ok(true)
    }

    /// Who commits; `""` clears.
    pub fn set_identity(&self, id: GitIdentity) -> Result<(), String> {
        let clean = |v: String| -> Result<Option<String>, String> {
            let v = v.trim().to_owned();
            if v.len() > 200 || v.chars().any(char::is_control) {
                return Err("a name or an email address on one line".into());
            }
            Ok(Some(v).filter(|v| !v.is_empty()))
        };
        let name = id.name.map(clean).transpose()?;
        let email = id.email.map(clean).transpose()?;
        if let Some(Some(e)) = &email
            && !e.contains('@')
        {
            return Err(format!("`{e}` isn't an email address"));
        }
        self.update(|a| {
            if let Some(n) = name {
                a.name = n;
            }
            if let Some(e) = email {
                a.email = e;
            }
        })
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
    }

    /// Otter's key alone: nothing from the host's `~/.ssh/config`, whose
    /// `IdentityFile`s ssh would try after it (and sign in as the host's
    /// account), and no agent.
    fn ssh_options(&self) -> Vec<String> {
        vec![
            "-F".into(),
            "/dev/null".into(),
            "-i".into(),
            self.key().display().to_string(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "IdentityAgent=none".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", self.known_hosts().display()),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
        ]
    }

    /// The ssh git runs in Otter's repositories once the key has signed in
    /// to GitHub: the script that routes GitHub to the key.
    pub fn ssh_command(&self) -> Option<String> {
        self.ssh_command_for(&self.account.read().unwrap())
    }

    fn ssh_command_for(&self, a: &Account) -> Option<String> {
        (a.key_account.is_some() && self.key().exists())
            .then(|| sh_quote(&self.dir.join("ssh").display().to_string()))
    }

    /// `gitconfig` (the identity, and the key once it works) and the `ssh`
    /// script it runs: Otter's key for GitHub alone, and the host's ssh as
    /// it is for any other remote (a repository's sshCommand is for all of
    /// its remotes).
    fn write_config(&self, a: &Account) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        private_dir(&self.dir)?;
        let options: Vec<String> = self.ssh_options().iter().map(|o| sh_quote(o)).collect();
        let script = format!(
            "#!/bin/sh\n# Written by Otter (D-132): its key for GitHub, the host's ssh for the rest.\nfor a in \"$@\"; do\n  case \"$a\" in\n    github.com|*@github.com|ssh.github.com|*@ssh.github.com) exec ssh {} \"$@\" ;;\n  esac\ndone\nexec ssh \"$@\"\n",
            options.join(" ")
        );
        // Executable from the moment it's in place: a push may run it then.
        let path = self.dir.join("ssh");
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, script.as_bytes())?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
        std::fs::rename(&tmp, &path)?;
        crate::settings::write_private(
            &self.config_file(),
            config_text(a, self.ssh_command_for(a)).as_bytes(),
        )
    }

    /// GitHub's own host key in Otter's `known_hosts`, unless it has it.
    fn seed_known_hosts(&self) -> Result<()> {
        let mut have = std::fs::read_to_string(self.known_hosts()).unwrap_or_default();
        let before = have.len();
        for key in [GITHUB_HOST_KEY, GITHUB_443_HOST_KEY] {
            if !have.lines().any(|l| l.trim() == key) {
                have.push_str(key);
                have.push('\n');
            }
        }
        if have.len() != before {
            crate::settings::write_private(&self.known_hosts(), have.as_bytes())?;
        }
        Ok(())
    }

    /// The environment for Otter's own gh: its config directory once
    /// signed in, and no token from the environment; else as it was.
    pub fn gh_env(&self, env: &EnvMap) -> EnvMap {
        let mut env = env.clone();
        if self.signed_in() {
            self.scope(&mut env);
        }
        env
    }

    fn scope(&self, env: &mut EnvMap) {
        for k in GH_OVERRIDES {
            env.remove(*k);
        }
        env.insert("GH_CONFIG_DIR".into(), self.gh_dir().display().to_string());
    }

    /// Make the key (or a new one with `replace`, which GitHub then has to
    /// learn again).
    pub async fn key_create(&self, replace: bool, env: &EnvMap) -> Result<()> {
        if self.key().exists() && !replace {
            return Ok(());
        }
        private_dir(&self.dir)?;
        self.seed_known_hosts()?;
        let keygen = which("ssh-keygen", env)
            .ok_or_else(|| anyhow!("ssh-keygen isn't on this host's PATH"))?;
        let _ = std::fs::remove_file(self.key());
        let _ = std::fs::remove_file(self.public_key());
        let comment = format!("otter@{}", crate::daemon::hostname());
        let out = Command::new(&keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
            .arg(self.key())
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .output()
            .await
            .context("running ssh-keygen")?;
        if !out.status.success() {
            bail!(
                "ssh-keygen: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let fingerprint = Command::new(&keygen)
            .arg("-lf")
            .arg(self.public_key())
            .env_clear()
            .envs(env)
            .output()
            .await
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .nth(1)
                    .map(str::to_owned)
            });
        self.update(|a| {
            a.key_account = None;
            a.fingerprint = fingerprint;
        })?;
        Ok(())
    }

    /// Sign Otter's gh in: with `token` at once, else in the browser (its
    /// code in the view's `login` while it waits).
    pub async fn login(self: &Arc<Self>, token: Option<String>, env: &EnvMap) -> Result<()> {
        let gh = which("gh", env).ok_or_else(|| {
            anyhow!("GitHub CLI (gh) isn't on this host's PATH: install it to sign in")
        })?;
        self.cancel_login().await;
        private_dir(&self.dir)?;
        private_dir(&self.gh_dir())?;
        let mut env = env.clone();
        self.scope(&mut env);
        let mut cmd = Command::new(&gh);
        cmd.args([
            "auth",
            "login",
            "--hostname",
            "github.com",
            "--git-protocol",
            "ssh",
            "--insecure-storage",
        ])
        .env_clear()
        .envs(&env)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        if let Some(token) = token {
            cmd.arg("--with-token");
            let mut child = cmd.spawn().context("starting gh")?;
            let mut stdin = child.stdin.take().expect("piped");
            stdin
                .write_all(format!("{}\n", token.trim()).as_bytes())
                .await?;
            drop(stdin);
            let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
                .await
                .map_err(|_| anyhow!("gh didn't finish signing in"))??;
            if !out.status.success() {
                bail!("{}", gh_error(&String::from_utf8_lossy(&out.stderr)));
            }
            return self.confirm(&gh, &env).await;
        }
        // The browser: gh prints its code and the page, then waits.
        cmd.args(["--web", "--skip-ssh-key", "--scopes", "admin:public_key"]);
        let mut child = cmd.spawn().context("starting gh")?;
        drop(child.stdin.take());
        let mut lines = BufReader::new(child.stderr.take().expect("piped")).lines();
        let (mut code, mut url, mut seen) = (None, None, String::new());
        let read = async {
            while let Ok(Some(line)) = lines.next_line().await {
                if code.is_none() {
                    code = device_code(&line);
                }
                if url.is_none() {
                    url = device_url(&line);
                }
                seen.push_str(&line);
                seen.push('\n');
                if code.is_some() && url.is_some() {
                    break;
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(30), read).await;
        let (Some(code), Some(url)) = (code, url) else {
            let _ = child.kill().await;
            bail!("{}", gh_error(&seen));
        };
        *self.login.lock().unwrap() = Some(GitHubLogin::Pending { code, url });
        let me = self.clone();
        let task = tokio::spawn(async move {
            let mut rest = String::new();
            let finished = tokio::time::timeout(LOGIN_WAIT, async {
                while let Ok(Some(line)) = lines.next_line().await {
                    rest.push_str(&line);
                    rest.push('\n');
                }
                child.wait().await
            })
            .await;
            let outcome = match finished {
                Ok(Ok(status)) if status.success() => {
                    me.confirm(&gh, &env).await.map_err(|e| format!("{e:#}"))
                }
                Ok(Ok(_)) => Err(gh_error(&rest)),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => {
                    Err("The sign-in wasn't finished within 15 minutes: start it again.".into())
                }
            };
            *me.login.lock().unwrap() = outcome.err().map(|error| GitHubLogin::Failed { error });
        });
        *self.waiting.lock().unwrap() = Some(task);
        Ok(())
    }

    /// Stop a browser sign-in that waits, and wait until its gh is gone:
    /// it mustn't finish after this (and write a token) behind our back.
    async fn cancel_login(&self) {
        let task = self.waiting.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        *self.login.lock().unwrap() = None;
    }

    /// gh signed in: who it is, or, if that can't be told, not signed in
    /// after all (never a token Otter doesn't know the account of).
    async fn confirm(&self, gh: &Path, env: &EnvMap) -> Result<()> {
        if let Err(e) = self.signed_in_as(gh, env).await {
            let _ = std::fs::remove_dir_all(self.gh_dir());
            let _ = self.update(|a| a.github = None);
            return Err(e.context("signed in, but GitHub didn't say who: not kept"));
        }
        Ok(())
    }

    /// Who gh says it is; kept for the view.
    async fn signed_in_as(&self, gh: &Path, env: &EnvMap) -> Result<String> {
        let (ok, out, err) =
            crate::delivery::run(gh, &["api", "user", "--jq", ".login"], &self.dir, env, None)
                .await?;
        let login = out.trim().to_owned();
        if !ok || login.is_empty() {
            bail!("{}", gh_error(&err));
        }
        self.update(|a| a.github = Some(login.clone()))?;
        Ok(login)
    }

    /// Sign Otter's gh out: its config directory goes.
    pub async fn logout(&self) -> Result<()> {
        self.cancel_login().await;
        match std::fs::remove_dir_all(self.gh_dir()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.update(|a| a.github = None)?;
        Ok(())
    }

    /// Add the key to the GitHub account gh is signed in to.
    pub async fn key_add(&self, env: &EnvMap) -> Result<()> {
        if !self.public_key().exists() {
            bail!("Make a key first.");
        }
        if !self.signed_in() {
            bail!("Sign in to GitHub first, or add the key on GitHub yourself.");
        }
        let gh =
            which("gh", env).ok_or_else(|| anyhow!("GitHub CLI (gh) isn't on this host's PATH"))?;
        let env = self.gh_env(env);
        let title = format!("Otter on {}", crate::daemon::hostname());
        let key = self.public_key().display().to_string();
        let (ok, _, err) = crate::delivery::run(
            &gh,
            &[
                "ssh-key",
                "add",
                &key,
                "--title",
                &title,
                "--type",
                "authentication",
            ],
            &self.dir,
            &env,
            None,
        )
        .await?;
        if ok || err.contains("already exists") {
            return Ok(());
        }
        if err.contains("key is already in use") {
            bail!("GitHub already has this key, on another account.");
        }
        if err.contains("403") || err.contains("admin:public_key") || err.contains("scope") {
            bail!(
                "Otter's GitHub sign-in can't add keys: sign in again here, or add the key on GitHub (Settings › SSH keys)."
            );
        }
        bail!("{}", gh_error(&err))
    }

    /// Whether the key and gh work with GitHub, and as whom. A key GitHub
    /// turns away stops being used; one GitHub can't be reached for stays
    /// as it was.
    pub async fn check(&self, env: &EnvMap) -> (CheckLine, CheckLine) {
        let ssh = self.check_ssh(env).await;
        let github = self.check_gh(env).await;
        let ssh = match (&ssh.account, &github.account) {
            (Some(k), Some(g)) if k != g => CheckLine {
                detail: format!("{} Otter's GitHub sign-in is {g}.", ssh.detail),
                ..ssh
            },
            _ => ssh,
        };
        (ssh, github)
    }

    async fn check_ssh(&self, env: &EnvMap) -> CheckLine {
        if !self.key().exists() {
            return line(false, "No key yet.", None);
        }
        let Some(ssh) = which("ssh", env) else {
            return line(false, "ssh isn't on this host's PATH.", None);
        };
        let _ = self.seed_known_hosts();
        let run = Command::new(ssh)
            .args(self.ssh_options())
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-T",
                GITHUB_SSH,
            ])
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(Duration::from_secs(30), run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return line(false, &format!("ssh didn't run: {e}"), None),
            Err(_) => return line(false, "GitHub didn't answer within 30 s.", None),
        };
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if let Some(account) = greeting(&text) {
            if let Err(e) = self.update(|a| a.key_account = Some(account.clone())) {
                return line(false, &format!("{e:#}"), None);
            }
            return line(
                true,
                &format!("The key signs in to GitHub as {account}."),
                Some(account),
            );
        }
        if text.contains("Permission denied") {
            let _ = self.update(|a| a.key_account = None);
            return line(
                false,
                "GitHub doesn't know this key yet: add it to your account.",
                None,
            );
        }
        line(
            false,
            &format!("Couldn't reach GitHub: {}", excerpt(&text)),
            None,
        )
    }

    async fn check_gh(&self, env: &EnvMap) -> CheckLine {
        if !self.signed_in() {
            return line(false, "Not signed in.", None);
        }
        let Some(gh) = which("gh", env) else {
            return line(false, "GitHub CLI (gh) isn't on this host's PATH.", None);
        };
        let env = self.gh_env(env);
        match self.signed_in_as(&gh, &env).await {
            Ok(login) => line(true, &format!("Signed in as {login}."), Some(login)),
            Err(e) => line(false, &format!("{e:#}"), None),
        }
    }
}

fn line(ok: bool, detail: &str, account: Option<String>) -> CheckLine {
    CheckLine {
        ok,
        detail: detail.into(),
        account,
    }
}

fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A value in a gitconfig file: quoted, `\` and `"` escaped.
fn config_value(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

fn config_text(a: &Account, ssh: Option<String>) -> String {
    let mut s = String::from(
        "# Written by Otter (D-132): what its repositories use. Edits are replaced.\n",
    );
    if a.name.is_some() || a.email.is_some() {
        s.push_str("[user]\n");
        if let Some(n) = &a.name {
            s.push_str(&format!("\tname = {}\n", config_value(n)));
        }
        if let Some(e) = &a.email {
            s.push_str(&format!("\temail = {}\n", config_value(e)));
        }
    }
    if let Some(ssh) = ssh {
        s.push_str(&format!("[core]\n\tsshCommand = {}\n", config_value(&ssh)));
        s.push_str("[url \"git@github.com:\"]\n\tpushInsteadOf = https://github.com/\n");
    }
    s
}

/// `! First copy your one-time code: ABCD-1234`.
fn device_code(line: &str) -> Option<String> {
    let rest = line.split("one-time code:").nth(1)?.trim();
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (code.len() >= 8).then_some(code)
}

/// The page to enter it at.
fn device_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let url: String = line[start..]
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    url.contains("/login/device").then_some(url)
}

/// `Hi alvin! You've successfully authenticated, …`.
fn greeting(text: &str) -> Option<String> {
    let rest = text.split("Hi ").nth(1)?;
    let (account, after) = rest.split_once('!')?;
    (after.contains("successfully authenticated")
        && !account.is_empty()
        && !account.contains(char::is_whitespace))
    .then(|| account.to_owned())
}

fn excerpt(text: &str) -> String {
    crate::agents::excerpt(&otter_core::redact::redact(text.trim()), 300)
}

/// What gh said, for people: never a token, and plainly when it's too old.
fn gh_error(stderr: &str) -> String {
    if stderr.contains("unknown flag") {
        return "This host's GitHub CLI is too old for Otter's sign-in: update gh (2.40 or newer)."
            .into();
    }
    let text = excerpt(stderr);
    if text.is_empty() {
        "gh failed without saying why.".into()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_gh_and_github_say() {
        assert_eq!(
            device_code("! First copy your one-time code: EFB1-6B49").as_deref(),
            Some("EFB1-6B49")
        );
        assert_eq!(
            device_url(
                "Open this URL to continue in your web browser: https://github.com/login/device"
            )
            .as_deref(),
            Some("https://github.com/login/device")
        );
        assert_eq!(device_url("see https://github.com/settings"), None);
        assert_eq!(
            greeting("Hi alvingaopro! You've successfully authenticated, but GitHub does not provide shell access.").as_deref(),
            Some("alvingaopro")
        );
        assert_eq!(
            greeting("git@github.com: Permission denied (publickey)."),
            None
        );
        assert!(gh_error("unknown flag: --insecure-storage").contains("update gh"));
    }

    #[test]
    fn writes_the_identity_and_the_key_once_it_works() {
        let a = Account {
            name: Some(r#"Alvin "A" Gao\"#.into()),
            email: Some("alvin@example.com".into()),
            ..Default::default()
        };
        let text = config_text(&a, None);
        assert!(text.contains(r#"name = "Alvin \"A\" Gao\\""#), "{text}");
        assert!(text.contains(r#"email = "alvin@example.com""#));
        assert!(!text.contains("sshCommand") && !text.contains("pushInsteadOf"));
        let text = config_text(&a, Some("ssh -i '/s/git/id_ed25519'".into()));
        assert!(
            text.contains(r#"sshCommand = "ssh -i '/s/git/id_ed25519'""#),
            "{text}"
        );
        assert!(text.contains("[url \"git@github.com:\"]\n\tpushInsteadOf = https://github.com/"));
        assert_eq!(config_text(&Account::default(), None).lines().count(), 1);
    }

    #[test]
    fn its_ssh_script_routes_github_alone_to_the_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let account = GitAccount::load(dir.path());
        let script = std::fs::read_to_string(account.dir.join("ssh")).unwrap();
        assert!(
            script.contains("github.com|*@github.com|ssh.github.com|*@ssh.github.com) exec ssh '-F' '/dev/null' '-i' "),
            "{script}"
        );
        assert!(script.trim_end().ends_with("exec ssh \"$@\""), "{script}");
        let mode = std::fs::metadata(account.dir.join("ssh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        // Not used until the key has signed in.
        assert_eq!(account.ssh_command(), None);
    }

    #[test]
    fn its_gh_never_takes_a_token_from_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let account = GitAccount::load(dir.path());
        let env = EnvMap::from([
            ("GH_TOKEN".to_owned(), "ghp_x".to_owned()),
            ("PATH".to_owned(), "/bin".to_owned()),
        ]);
        // Not signed in: the host's own gh, as it was.
        assert_eq!(account.gh_env(&env), env);
        std::fs::create_dir_all(account.gh_dir()).unwrap();
        std::fs::write(account.gh_dir().join("hosts.yml"), "github.com:\n").unwrap();
        let scoped = account.gh_env(&env);
        assert!(!scoped.contains_key("GH_TOKEN"));
        assert_eq!(
            scoped["GH_CONFIG_DIR"],
            account.gh_dir().display().to_string()
        );
    }

    #[test]
    fn an_identity_is_one_line_and_an_address() {
        let dir = tempfile::tempdir().unwrap();
        let account = GitAccount::load(dir.path());
        let set = |name: &str, email: &str| {
            account.set_identity(GitIdentity {
                name: Some(name.into()),
                email: Some(email.into()),
            })
        };
        assert!(set("Alvin", "nope").is_err());
        assert!(set("Al\nvin", "a@b.c").is_err());
        set(" Alvin ", "a@b.c").unwrap();
        let v = account.view();
        assert_eq!(
            (v.name.as_deref(), v.email.as_deref()),
            (Some("Alvin"), Some("a@b.c"))
        );
        assert!(
            std::fs::read_to_string(account.config_file())
                .unwrap()
                .contains("email = \"a@b.c\"")
        );
        // Cleared: the host's own config applies again.
        set("", "").unwrap();
        assert!(
            !std::fs::read_to_string(account.config_file())
                .unwrap()
                .contains("[user]")
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&account.dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
