//! Browser login: a CLI on the host (`aws sso login`, `gcloud auth login`,
//! `gh auth login`, …) that wants to open a sign-in page gets it opened in
//! the Otter app's browser on the Mac (or by an `otter attach` there), and a
//! loopback callback port it waits on is mapped from the Mac for the few
//! minutes the login takes.
//!
//! Tools open browsers through `BROWSER`, `xdg-open` or `www-browser`; the
//! stand-ins in `run/bin` (first on every session's PATH, see `files.rs`)
//! run `otterd open-url <url>`. The policy follows portkeeper's:
//!
//! - **https only**, and only **trusted sign-in providers** ([`PROVIDERS`]).
//! - **One loopback port** at most: the explicit port of a `redirect_uri`
//!   to `127.0.0.1`/`localhost`/`[::1]`, found in the URL or in an https URL
//!   wrapped inside it (two levels).
//! - Refused when no Otter app or attached terminal is connected, so the tool
//!   prints the URL as it would without a browser.
//!
//! The URL itself never goes into `events.jsonl`: the event names only the
//! provider and the port; the app (or terminal) takes the URL with
//! `browser.take`, once.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{Duration, Utc};
use otter_core::Timestamp;
use url::Url;

/// Sign-in hosts that open without asking: a host, or `*.` plus a domain for
/// any subdomain, `*` as one whole label, optionally a path prefix.
pub const PROVIDERS: &[&str] = &[
    "oidc.*.amazonaws.com",
    "device.sso.*.amazonaws.com",
    "*.awsapps.com",
    "accounts.google.com",
    "login.microsoftonline.com",
    "microsoft.com/devicelogin",
    "github.com/login",
    "app.terraform.io",
];

/// How long a request waits for an app to take it.
const PENDING_TTL: Duration = Duration::minutes(2);

/// An accepted sign-in page.
#[derive(Clone, Debug, PartialEq)]
pub struct Opening {
    pub url: String,
    pub provider: String,
    pub callback_port: Option<u16>,
}

/// Decide whether `raw` may be opened on the Mac.
pub fn check(raw: &str) -> Result<Opening, String> {
    let url = Url::parse(raw).map_err(|_| "not a URL".to_owned())?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err("only https sign-in pages are opened on the Mac".into());
    }
    let host = url.host_str().unwrap_or_default().to_lowercase();
    if !trusted(&url) {
        return Err(format!("{host} is not a trusted sign-in provider in Otter"));
    }
    Ok(Opening {
        url: raw.to_owned(),
        provider: host,
        callback_port: loopback_port(&url, 2),
    })
}

fn trusted(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default().to_lowercase();
    let path = url.path().trim_start_matches('/').to_lowercase();
    PROVIDERS.iter().any(|entry| {
        let (pat, prefix) = entry.split_once('/').unwrap_or((entry, ""));
        host_matches(&host, pat) && path.starts_with(prefix)
    })
}

fn host_matches(host: &str, pat: &str) -> bool {
    if let Some(rest) = pat.strip_prefix("*.") {
        return host == rest || host.ends_with(&format!(".{rest}"));
    }
    let (h, p): (Vec<&str>, Vec<&str>) = (host.split('.').collect(), pat.split('.').collect());
    h.len() == p.len() && p.iter().zip(&h).all(|(p, h)| *p == "*" || p == h)
}

/// The port of a loopback `redirect_uri`, looking into https URLs wrapped in
/// the query (`continue`, `return_to`, …) up to `depth` levels.
fn loopback_port(url: &Url, depth: u32) -> Option<u16> {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    if let Some(p) = pairs
        .iter()
        .filter(|(k, _)| k == "redirect_uri")
        .find_map(|(_, v)| loopback_redirect(v))
    {
        return Some(p);
    }
    if depth == 0 {
        return None;
    }
    let mut sorted = pairs;
    sorted.sort();
    sorted.iter().find_map(|(_, v)| {
        let inner = Url::parse(v).ok()?;
        (inner.scheme() == "https" && inner.host_str().is_some())
            .then(|| loopback_port(&inner, depth - 1))
            .flatten()
    })
}

fn loopback_redirect(raw: &str) -> Option<u16> {
    let u = Url::parse(raw).ok()?;
    if u.scheme() != "http" {
        return None;
    }
    match u.host_str()? {
        "127.0.0.1" | "localhost" | "[::1]" | "::1" => {}
        _ => return None,
    }
    // An explicit, unprivileged port only (`url` drops the default 80).
    u.port().filter(|p| *p >= 1024)
}

// ---------------------------------------------------------------------------
// Requests waiting for an app
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Pending {
    inner: Mutex<HashMap<String, (Opening, Timestamp)>>,
}

impl Pending {
    pub fn add(&self, id: String, opening: Opening) {
        let mut inner = self.inner.lock().unwrap();
        let now = Utc::now();
        inner.retain(|_, (_, at)| now - *at < PENDING_TTL);
        inner.insert(id, (opening, now));
    }

    /// Hand a request to an app, once.
    pub fn take(&self, id: &str) -> Option<Opening> {
        let (opening, at) = self.inner.lock().unwrap().remove(id)?;
        (Utc::now() - at < PENDING_TTL).then_some(opening)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_trusted_https_providers_only() {
        assert!(check("https://d-1234.awsapps.com/start/#/device?user_code=ABCD").is_ok());
        assert!(check("https://oidc.us-east-1.amazonaws.com/authorize?client_id=x").is_ok());
        assert!(check("https://accounts.google.com/o/oauth2/auth?x=1").is_ok());
        assert!(check("https://github.com/login/device").is_ok());
        assert!(check("https://microsoft.com/devicelogin").is_ok());
        // Not a provider, wrong path, not https, look-alike hosts.
        assert!(
            check("https://example.com/")
                .unwrap_err()
                .contains("not a trusted")
        );
        assert!(check("https://github.com/evil/repo").is_err());
        assert!(
            check("http://accounts.google.com/")
                .unwrap_err()
                .contains("https")
        );
        assert!(check("https://accounts.google.com.evil.com/").is_err());
        assert!(check("https://oidc.amazonaws.com.evil.com/").is_err());
        assert!(check("file:///etc/passwd").is_err());
    }

    #[test]
    fn finds_the_loopback_callback_port() {
        let aws = "https://oidc.us-east-1.amazonaws.com/authorize?response_type=code&client_id=x&redirect_uri=http%3A%2F%2F127.0.0.1%3A37265%2Foauth%2Fcallback&state=s";
        assert_eq!(check(aws).unwrap().callback_port, Some(37265));
        let gcloud = "https://accounts.google.com/o/oauth2/auth?redirect_uri=http%3A%2F%2Flocalhost%3A8085%2F&client_id=y";
        assert_eq!(check(gcloud).unwrap().callback_port, Some(8085));
        // Wrapped in a "continue" URL.
        let inner = "https://accounts.google.com/o/oauth2/auth?redirect_uri=http%3A%2F%2Flocalhost%3A9000%2F";
        let outer = format!(
            "https://accounts.google.com/ServiceLogin?continue={}",
            url::form_urlencoded::byte_serialize(inner.as_bytes()).collect::<String>()
        );
        assert_eq!(check(&outer).unwrap().callback_port, Some(9000));
        // Device code (no redirect), a remote redirect, a privileged port: none.
        assert_eq!(
            check("https://github.com/login/device")
                .unwrap()
                .callback_port,
            None
        );
        let remote = "https://accounts.google.com/x?redirect_uri=http%3A%2F%2Fevil.com%3A2222%2F";
        assert_eq!(check(remote).unwrap().callback_port, None);
        let low = "https://accounts.google.com/x?redirect_uri=http%3A%2F%2F127.0.0.1%3A22%2F";
        assert_eq!(check(low).unwrap().callback_port, None);
    }

    #[test]
    fn pending_requests_are_taken_once() {
        let p = Pending::default();
        p.add(
            "r1".into(),
            check("https://github.com/login/device").unwrap(),
        );
        assert!(p.take("r1").is_some());
        assert!(p.take("r1").is_none());
    }
}
