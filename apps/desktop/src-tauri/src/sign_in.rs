//! Sign-in pages from hosts (D-032, D-121): which sites this Mac opens
//! without asking, kept in `hosts.toml` with the hosts (the CLI reads them
//! too), and the question asked about any other site.
//!
//! A host offers a page on a site that isn't built in (`BrowserOpenAsked`).
//! If the whole site is one allowed here, it opens; otherwise the window asks
//! ("sign-in-ask"), and the developer opens it once or always.

use otter_core::WorkspaceId;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::hosts::load_config;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInSites {
    /// Who's built in, by name.
    built_in: &'static [&'static str],
    /// Sites allowed on this Mac, as kept.
    sites: Vec<String>,
}

/// A page waiting for the developer's answer.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SignInAsk<'a> {
    host: &'a str,
    request_id: &'a str,
    site: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<&'a WorkspaceId>,
}

fn sites_now() -> Vec<String> {
    load_config().map(|c| c.sign_in_sites).unwrap_or_default()
}

#[tauri::command]
pub fn sign_in_sites() -> SignInSites {
    SignInSites {
        built_in: otter_protocol::sign_in::BUILT_IN_NAMES,
        sites: sites_now(),
    }
}

/// Open pages on `site` (a site, or a link pasted) without asking.
#[tauri::command]
pub fn sign_in_allow(site: String) -> Result<Vec<String>, String> {
    let mut config = load_config().map_err(|e| format!("{e:#}"))?;
    config
        .allow_sign_in_site(&site)
        .map_err(|e| e.to_string())?;
    config.save().map_err(|e| format!("{e:#}"))?;
    Ok(config.sign_in_sites)
}

/// Ask again before opening pages on `site`.
#[tauri::command]
pub fn sign_in_forget(site: String) -> Result<Vec<String>, String> {
    let mut config = load_config().map_err(|e| format!("{e:#}"))?;
    config.forget_sign_in_site(&site);
    config.save().map_err(|e| format!("{e:#}"))?;
    Ok(config.sign_in_sites)
}

/// The developer said open: this page, and with `always` every page on its
/// site from now on.
#[tauri::command]
pub fn sign_in_open(
    app: AppHandle,
    host: String,
    request_id: String,
    site: String,
    always: bool,
) -> Result<(), String> {
    if always {
        sign_in_allow(site.clone())?;
    }
    tauri::async_runtime::spawn(crate::forwards::open_login(
        app,
        host,
        request_id,
        Some(site),
    ));
    Ok(())
}

/// A host offered a page on a site that isn't built in.
pub fn asked(
    app: &AppHandle,
    host: &str,
    request_id: &str,
    site: &str,
    workspace_id: Option<&WorkspaceId>,
) {
    if otter_protocol::sign_in::whole_site_on(site, &sites_now()) {
        tauri::async_runtime::spawn(crate::forwards::open_login(
            app.clone(),
            host.to_owned(),
            request_id.to_owned(),
            None,
        ));
        return;
    }
    let _ = app.emit(
        "sign-in-ask",
        SignInAsk {
            host,
            request_id,
            site,
            workspace_id,
        },
    );
}
