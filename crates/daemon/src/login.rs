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
//! - **https only**. A provider built in
//!   ([`otter_protocol::sign_in::BUILT_IN`]) opens at once; any other site is
//!   offered to the Mac, which opens it only if the developer allowed that
//!   site there, or asks (D-121). Either way the Mac checks the page again.
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

/// How long a request waits for an app to take it.
const PENDING_TTL: Duration = Duration::minutes(2);

/// A sign-in page that may go to the Mac.
#[derive(Clone, Debug, PartialEq)]
pub struct Opening {
    pub url: String,
    /// The page's host.
    pub provider: String,
    pub callback_port: Option<u16>,
    /// A provider built in: it opens at once. Otherwise the Mac decides.
    pub built_in: bool,
}

/// Whether `raw` can go to the Mac at all: an https page.
pub fn check(raw: &str) -> Result<Opening, String> {
    let url = Url::parse(raw).map_err(|_| "not a URL".to_owned())?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err("only https sign-in pages are opened on the Mac".into());
    }
    // A named host, never a pattern (`https://*.co.uk/` parses): the site the
    // Mac is told about is what *Always open* would allow.
    let host = match url.host() {
        Some(url::Host::Domain(d)) if otter_protocol::sign_in::is_host(d) => d.to_lowercase(),
        _ => return Err("only sign-in pages on a named host are opened on the Mac".into()),
    };
    let none: &[&str] = &[];
    Ok(Opening {
        url: raw.to_owned(),
        provider: host,
        callback_port: otter_protocol::sign_in::callback_port(&url),
        built_in: otter_protocol::sign_in::opens(&url, none),
    })
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
    fn built_in_providers_open_at_once_others_go_to_the_mac() {
        for page in [
            "https://d-1234.awsapps.com/start/#/device?user_code=ABCD",
            "https://oidc.us-east-1.amazonaws.com/authorize?client_id=x",
            "https://accounts.google.com/o/oauth2/auth?x=1",
            "https://github.com/login/device",
            "https://microsoft.com/devicelogin",
            "https://login.salesforce.com/services/oauth2/authorize?x=1",
            "https://x-dev-ed.scratch.my.salesforce.com/secur/frontdoor.jsp?sid=s",
        ] {
            assert!(check(page).unwrap().built_in, "{page}");
        }
        // Not a provider, wrong path, look-alike hosts: the Mac decides.
        for page in [
            "https://example.com/",
            "https://github.com/evil/repo",
            "https://accounts.google.com.evil.com/",
            "https://oidc.amazonaws.com.evil.com/",
        ] {
            let opening = check(page).unwrap();
            assert!(!opening.built_in, "{page}");
            assert_eq!(
                opening.provider,
                Url::parse(page).unwrap().host_str().unwrap()
            );
        }
        // Not https, or not a named host: never.
        assert!(
            check("http://accounts.google.com/")
                .unwrap_err()
                .contains("https")
        );
        assert!(check("https://*.co.uk/x").is_err(), "a pattern for a host");
        assert!(check("https://10.0.0.1/login").is_err());
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
