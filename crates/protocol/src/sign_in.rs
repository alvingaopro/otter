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
    Url::parse(&format!("https://{site}/")).is_ok_and(|url| on(&url, allowed))
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
        return checked(url.host_str()?.to_owned());
    }
    checked(input.trim_end_matches('/').to_owned())
}

fn checked(site: String) -> Option<String> {
    let (host, path) = site.split_once('/').unwrap_or((&site, ""));
    let labels: Vec<&str> = host.split('.').collect();
    let wild = labels.first() == Some(&"*");
    let named = &labels[usize::from(wild)..];
    let ok = named.len() >= 2
        && named.iter().all(|l| {
            !l.is_empty()
                && l.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '*')
        })
        && !path.contains(char::is_whitespace);
    ok.then_some(site)
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
        ] {
            assert_eq!(site(bad), None, "{bad:?}");
        }
    }
}
