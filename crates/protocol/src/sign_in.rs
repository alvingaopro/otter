//! Which sign-in pages open on the Mac (D-032, D-121): the providers built
//! in, and the sites the developer allowed on the Mac. The daemon uses the
//! built-in list to open a page at once or ask; the Mac checks a page again
//! before opening it, with its own list too.
//!
//! A site is a host (`login.example.com`), `*.` and a domain for it and any
//! subdomain (`*.example.com`), `*` as one whole label
//! (`oidc.*.amazonaws.com`), and maybe a path prefix (`github.com/login`).

use url::Url;

/// Sign-in providers that open without asking.
pub const BUILT_IN: &[&str] = &[
    "oidc.*.amazonaws.com",
    "device.sso.*.amazonaws.com",
    "*.awsapps.com",
    "accounts.google.com",
    "login.microsoftonline.com",
    "microsoft.com/devicelogin",
    "github.com/login",
    "app.terraform.io",
    // `sf org login web` and `sf org open` (orgs, scratch orgs, sandboxes).
    "login.salesforce.com",
    "test.salesforce.com",
    "*.my.salesforce.com",
];

/// Who [`BUILT_IN`] is, for people.
pub const BUILT_IN_NAMES: &[&str] = &[
    "AWS",
    "Google",
    "Microsoft",
    "GitHub",
    "HashiCorp",
    "Salesforce",
];

/// Whether `url` is an https page on one of `sites`.
pub fn on(url: &Url, sites: &[impl AsRef<str>]) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    let host = url.host_str().unwrap_or_default().to_lowercase();
    let path = url.path().trim_start_matches('/').to_lowercase();
    sites.iter().any(|site| {
        let site = site.as_ref();
        let (pat, prefix) = site.split_once('/').unwrap_or((site, ""));
        host_matches(&host, pat) && path.starts_with(prefix)
    })
}

/// Whether `url` opens on this Mac: a provider built in, or one of `allowed`.
pub fn opens(url: &Url, allowed: &[impl AsRef<str>]) -> bool {
    on(url, BUILT_IN) || on(url, allowed)
}

/// Whether every page on `site` (a host) is on one of `allowed`: what the
/// Mac opens without asking when a page there is offered.
pub fn whole_site_on(site: &str, allowed: &[impl AsRef<str>]) -> bool {
    is_host(site) && Url::parse(&format!("https://{site}/")).is_ok_and(|url| on(&url, allowed))
}

/// Whether `s` is a plain host name: labels of letters, digits and `-`, as a
/// page's host is. Never a pattern (`*`) or a path: what a host says a page's
/// site is must be one, or a click on *Always open* could allow a pattern
/// the host chose (`*.co.uk`).
pub fn is_host(s: &str) -> bool {
    let labels: Vec<&str> = s.split('.').collect();
    labels.len() >= 2
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// The port of a loopback `redirect_uri` the sign-in sends back to, the
/// page's callback, looking into https URLs wrapped in the query
/// (`continue`, `return_to`, …) up to two levels. An explicit, unprivileged
/// port only. The Mac maps it from the page it opens, not from what a host
/// says.
pub fn callback_port(url: &Url) -> Option<u16> {
    loopback_port(url, 2)
}

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
    // `url` drops the default 80.
    u.port().filter(|p| *p >= 1024)
}

fn host_matches(host: &str, pat: &str) -> bool {
    if let Some(rest) = pat.strip_prefix("*.") {
        return host == rest || host.ends_with(&format!(".{rest}"));
    }
    let (h, p): (Vec<&str>, Vec<&str>) = (host.split('.').collect(), pat.split('.').collect());
    h.len() == p.len() && p.iter().zip(&h).all(|(p, h)| *p == "*" || p == h)
}

/// What the developer typed or pasted, as a site: a link gives its host;
/// `*.example.com`, a host, or a host with a path prefix stay as they are.
/// `None` if it can't be one (no dot, spaces, a scheme other than https).
pub fn site(input: &str) -> Option<String> {
    let input = input.trim().to_lowercase();
    if input.contains("://") {
        let url = Url::parse(&input).ok()?;
        if url.scheme() != "https" {
            return None;
        }
        // A link's host is a host: never a pattern.
        let host = url.host_str()?.to_owned();
        return is_host(&host).then_some(host);
    }
    checked(input.trim_end_matches('/').to_owned())
}

/// `*` only as the leading `*.`, typed by the developer; then a host.
fn checked(site: String) -> Option<String> {
    let (host, path) = site.split_once('/').unwrap_or((&site, ""));
    let named = host.strip_prefix("*.").unwrap_or(host);
    (is_host(named) && !path.contains(char::is_whitespace)).then_some(site)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn built_in_providers_open_others_only_if_allowed() {
        let none: &[&str] = &[];
        for page in [
            "https://oidc.us-east-1.amazonaws.com/authorize?x=1",
            "https://my-org.awsapps.com/start",
            "https://github.com/login/device",
            "https://login.salesforce.com/services/oauth2/authorize",
            "https://inspiration-dream-5410-dev-ed.scratch.my.salesforce.com/secur/frontdoor.jsp?sid=x",
        ] {
            assert!(opens(&url(page), none), "{page}");
        }
        for page in [
            "https://github.com/settings",
            "http://login.salesforce.com/",
            "https://evil.example/login",
            "https://my.salesforce.com.evil.example/",
        ] {
            assert!(!opens(&url(page), none), "{page}");
        }
        assert!(opens(&url("https://sso.example.com/a"), &["*.example.com"]));
        assert!(opens(&url("https://example.com/a"), &["*.example.com"]));
        assert!(!opens(
            &url("https://sso.example.com/a"),
            &["login.example.com"]
        ));
    }

    #[test]
    fn what_was_typed_or_pasted_becomes_a_site() {
        assert_eq!(
            site("https://Login.Example.com/oauth?x=1").as_deref(),
            Some("login.example.com")
        );
        assert_eq!(site("  *.example.com ").as_deref(), Some("*.example.com"));
        assert_eq!(
            site("github.com/login/").as_deref(),
            Some("github.com/login")
        );
        for bad in [
            "",
            "localhost",
            "http://example.com",
            "*.com",
            "a b.com",
            "ftp://x.com",
            "login.*.com",
            "a*b.example.com",
            "*.*.example.com",
            "https://*.co.uk/x",
        ] {
            assert_eq!(site(bad), None, "{bad:?}");
        }
        // What a host says a site is must be a host, never a pattern.
        assert!(is_host("x-dev-ed.scratch.my.salesforce.com"));
        for bad in ["*.co.uk", "co", "a.b/c", "a..b", ""] {
            assert!(!is_host(bad), "{bad:?}");
        }
        assert!(!whole_site_on("*.co.uk", &["*.co.uk"]));
    }

    #[test]
    fn finds_the_loopback_callback_port() {
        let port = |s: &str| callback_port(&url(s));
        let aws = "https://oidc.us-east-1.amazonaws.com/authorize?response_type=code&client_id=x&redirect_uri=http%3A%2F%2F127.0.0.1%3A37265%2Foauth%2Fcallback&state=s";
        assert_eq!(port(aws), Some(37265));
        let gcloud = "https://accounts.google.com/o/oauth2/auth?redirect_uri=http%3A%2F%2Flocalhost%3A8085%2F&client_id=y";
        assert_eq!(port(gcloud), Some(8085));
        // Wrapped in a "continue" URL.
        let inner = "https://accounts.google.com/o/oauth2/auth?redirect_uri=http%3A%2F%2Flocalhost%3A9000%2F";
        let outer = format!(
            "https://accounts.google.com/ServiceLogin?continue={}",
            url::form_urlencoded::byte_serialize(inner.as_bytes()).collect::<String>()
        );
        assert_eq!(port(&outer), Some(9000));
        // Device code (no redirect), a remote redirect, a privileged port: none.
        assert_eq!(port("https://github.com/login/device"), None);
        assert_eq!(
            port("https://accounts.google.com/x?redirect_uri=http%3A%2F%2Fevil.com%3A2222%2F"),
            None
        );
        assert_eq!(
            port("https://accounts.google.com/x?redirect_uri=http%3A%2F%2F127.0.0.1%3A22%2F"),
            None
        );
    }
}
