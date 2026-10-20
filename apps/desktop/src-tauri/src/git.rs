//! Otter's own git on every host (D-132): the identity, kept on this Mac in
//! `hosts.toml` (with the CLI's) and sent to each host as it connects; each
//! host's key and GitHub sign-in, driven from Settings › Git & GitHub.

use otter_client::Connection;
use otter_protocol::host::{GitCheck, GitIdentity, GitView, SettingsUpdate};
use serde::Serialize;
use tauri::AppHandle;

use crate::hosts::{load_config, rpc};

/// How sending the identity to one host went.
#[derive(Serialize)]
pub struct Sent {
    host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[tauri::command]
pub fn git_identity() -> GitIdentity {
    load_config().ok().and_then(|c| c.git).unwrap_or_default()
}

/// Keep `name` and `email` here and send them to every connected host
/// (`""` clears: the hosts' own git config applies again).
#[tauri::command]
pub async fn git_identity_set(
    app: AppHandle,
    hosts: Vec<String>,
    name: String,
    email: String,
) -> Result<Vec<Sent>, String> {
    let id = GitIdentity {
        name: Some(name.trim().to_owned()),
        email: Some(email.trim().to_owned()),
    };
    let mut sent = Vec::new();
    for host in hosts {
        let error = async {
            rpc(&app, &host)
                .await?
                .settings_set(SettingsUpdate {
                    git: Some(id.clone()),
                    ..Default::default()
                })
                .await
                .map_err(|e| e.to_string())
        }
        .await
        .err();
        sent.push(Sent { host, error });
    }
    // Kept once a host took it (or with none to send to): what a host
    // refuses (not an email address, say) isn't kept.
    if sent.iter().any(|s| s.error.is_none()) || sent.is_empty() {
        let mut config = load_config().map_err(|e| format!("{e:#}"))?;
        let empty = id.name.as_deref() == Some("") && id.email.as_deref() == Some("");
        config.git = (!empty).then_some(id);
        config.save().map_err(|e| format!("{e:#}"))?;
    }
    Ok(sent)
}

/// This Mac's identity on a host that just connected, if it has another.
pub async fn sync_identity(conn: &mut Connection) {
    let Some(id) = load_config().ok().and_then(|c| c.git) else {
        return;
    };
    // An otterd from before D-132 has no git settings.
    let Ok(Some(have)) = conn.settings_get().await.map(|s| s.git) else {
        return;
    };
    if have.name == id.name && have.email == id.email {
        return;
    }
    if let Err(e) = conn
        .settings_set(SettingsUpdate {
            git: Some(id),
            ..Default::default()
        })
        .await
    {
        eprintln!("sending the git identity: {e}");
    }
}

/// A host's git view; `None` from an otterd before D-132.
#[tauri::command]
pub async fn git_view(app: AppHandle, host: String) -> Result<Option<GitView>, String> {
    let s = rpc(&app, &host)
        .await?
        .settings_get()
        .await
        .map_err(|e| e.to_string())?;
    Ok(s.git)
}

#[tauri::command]
pub async fn git_key_create(
    app: AppHandle,
    host: String,
    replace: bool,
) -> Result<GitView, String> {
    rpc(&app, &host)
        .await?
        .git_key_create(replace)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn git_key_add(app: AppHandle, host: String) -> Result<GitCheck, String> {
    rpc(&app, &host)
        .await?
        .git_key_add()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn git_check(app: AppHandle, host: String) -> Result<GitCheck, String> {
    rpc(&app, &host)
        .await?
        .git_check()
        .await
        .map_err(|e| e.to_string())
}

/// Sign the host's Otter gh in: with `token`, or in the browser.
#[tauri::command]
pub async fn github_login(
    app: AppHandle,
    host: String,
    token: Option<String>,
) -> Result<GitView, String> {
    rpc(&app, &host)
        .await?
        .github_login(token.as_deref().filter(|t| !t.trim().is_empty()))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn github_logout(app: AppHandle, host: String) -> Result<GitView, String> {
    rpc(&app, &host)
        .await?
        .github_logout()
        .await
        .map_err(|e| e.to_string())
}
