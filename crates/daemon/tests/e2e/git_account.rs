//! Otter's own git on a host (D-132): the developer's identity in the
//! repositories Otter manages, a key made on the host and used once GitHub
//! knows it, and a GitHub sign-in for Otter's gh alone — the host's own git,
//! ssh and gh untouched.

use super::*;
use otter_protocol::host::{GitHubLogin, GitIdentity, SettingsUpdate};

/// A fake `ssh` to GitHub: it knows Otter's key once the fake gh added it.
const FAKE_SSH: &str = r#"#!/bin/sh
echo "ssh $*" >> "$FAKE_GH_DIR/ssh.log"
if [ -f "$FAKE_GH_DIR/added-key" ]; then
  echo "Hi alvin! You've successfully authenticated, but GitHub does not provide shell access." >&2; exit 1
fi
echo "git@github.com: Permission denied (publickey)." >&2; exit 255
"#;

fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap()
}

#[tokio::test]
async fn otters_git_identity_key_and_github_sign_in_are_its_own() {
    use std::os::unix::fs::PermissionsExt;
    let fake = fake_agent_host_with("ok", "off", |scratch| {
        let bin = scratch.join("sshbin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("ssh"), FAKE_SSH).unwrap();
        std::fs::set_permissions(bin.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = |x: &Path| x.to_string_lossy().into_owned();
        vec![
            (
                "PATH",
                format!(
                    "{}:{}:{}:/usr/local/bin:/usr/bin:/bin",
                    p(&bin),
                    p(&scratch.join("bin")),
                    tmux_dir()
                ),
            ),
            // The host's own gh token: Otter's gh never takes it.
            ("GH_TOKEN", "ghp_the_hosts_own".into()),
        ]
    })
    .await;
    let mut conn = fake.host.conn().await;
    let state = fake.host.home().join("state/git");

    // The developer's identity, in Otter's repositories only.
    let s = conn
        .settings_set(SettingsUpdate {
            git: Some(GitIdentity {
                name: Some("Alvin Gao".into()),
                email: Some("alvin@example.com".into()),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.git.unwrap().email.as_deref(), Some("alvin@example.com"));
    let src = make_repo(&fake.host, "src", &[("README.md", "hello\n")]);
    conn.workspace_create(git_ws("ident", &src, None, vec![]))
        .await
        .unwrap();
    let ws = wait_prepared(&fake.host, "ident").await;
    let root = Path::new(&ws.root);
    let out = git_in(root, &["commit", "--allow-empty", "-q", "-m", "by hand"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let who = git_in(root, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]);
    assert_eq!(
        String::from_utf8_lossy(&who.stdout).trim(),
        "Alvin Gao <alvin@example.com>|Alvin Gao <alvin@example.com>"
    );
    // Not in the source repository, which isn't Otter's.
    assert!(
        !git_in(&src, &["config", "--local", "user.email"])
            .status
            .success()
    );
    // No key in use before GitHub knows one.
    assert!(
        !git_in(root, &["config", "core.sshCommand"])
            .status
            .success()
    );

    // A key made on the host; its private half stays there.
    let v = conn.git_key_create(false).await.unwrap();
    let key = v.ssh_key.expect("a key");
    assert!(key.public.starts_with("ssh-ed25519 "), "{key:?}");
    assert!(
        key.fingerprint
            .as_deref()
            .unwrap_or("")
            .starts_with("SHA256:"),
        "{key:?}"
    );
    let private = state.join("id_ed25519");
    assert_eq!(
        std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let c = conn.git_check().await.unwrap();
    assert!(
        !c.ssh.ok && c.ssh.detail.contains("doesn't know this key"),
        "{c:?}"
    );
    assert!(!c.github.ok);
    assert!(
        !git_in(root, &["config", "core.sshCommand"])
            .status
            .success()
    );

    // Signed in with a token: a bad one says why; a good one, as whom.
    let err = conn.github_login(Some("bad")).await.unwrap_err();
    assert!(format!("{err}").contains("Bad credentials"), "{err}");
    let v = conn.github_login(Some("ghp_developers")).await.unwrap();
    assert_eq!(v.github.as_deref(), Some("alvin"));
    assert!(state.join("gh/hosts.yml").exists());

    // The key goes to that account; from then on the repositories push with it.
    let c = conn.git_key_add().await.unwrap();
    assert!(c.ssh.ok && c.github.ok, "{c:?}");
    assert_eq!(c.git.key_account.as_deref(), Some("alvin"));
    let ssh = git_in(root, &["config", "core.sshCommand"]);
    let ssh = String::from_utf8_lossy(&ssh.stdout);
    assert!(
        ssh.contains("id_ed25519")
            && ssh.contains("IdentitiesOnly=yes")
            && ssh.contains("IdentityAgent=none"),
        "{ssh}"
    );
    let push = git_in(root, &["config", "url.git@github.com:.pushInsteadOf"]);
    assert_eq!(
        String::from_utf8_lossy(&push.stdout).trim(),
        "https://github.com/"
    );
    // Host keys are Otter's to keep, not the host's.
    let log = std::fs::read_to_string(fake.gh.join("ssh.log")).unwrap();
    assert!(
        log.contains(&format!(
            "UserKnownHostsFile={}",
            state.join("known_hosts").display()
        )),
        "{log}"
    );

    // Signed out, then in through the browser: its code while it waits.
    let v = conn.github_logout().await.unwrap();
    assert!(v.github.is_none() && !state.join("gh").exists());
    let v = conn.github_login(None).await.unwrap();
    assert_eq!(
        v.login,
        Some(GitHubLogin::Pending {
            code: "EFB1-6B49".into(),
            url: "https://github.com/login/device".into()
        })
    );
    std::fs::write(fake.gh.join("approve"), "").unwrap();
    eventually("the browser sign-in finishes", || async {
        let g = fake
            .host
            .conn()
            .await
            .settings_get()
            .await
            .unwrap()
            .git
            .unwrap();
        (g.github.as_deref() == Some("alvin") && g.login.is_none()).then_some(())
    })
    .await;

    // Without an identity, the host's own applies again.
    conn.settings_set(SettingsUpdate {
        git: Some(GitIdentity {
            name: Some(String::new()),
            email: Some(String::new()),
        }),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(
        !git_in(root, &["config", "--includes", "--local", "user.email"])
            .status
            .success()
    );
}
