//! Credentials out of the text Otter keeps and shows (D-075, invariant 13).
//!
//! What a failing tool prints — direnv, tmux, git, the Claude worker — may
//! carry a secret: a token in a URL, an `Authorization` header, a
//! `TOKEN=…` line, a key in a known shape, a private key. Before such text
//! becomes Otter's (state, events, a conversation's journal, a task's
//! messages, otterd's log, a response), [`redact`] replaces the secret with
//! `***`, keeping enough around it to read the failure.
//!
//! Shapes, not a scanner: a secret in no known shape (a bare random string)
//! passes. Lexical, cheap, and idempotent.

const MASK: &str = "***";

/// Words in a key that say its value is a secret (`API_TOKEN=`, `password:`).
const SECRET_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "private_key",
    "client_secret",
    "credential",
];

/// Token shapes by prefix, with how many token characters must follow.
const TOKEN_PREFIXES: &[(&str, usize)] = &[
    ("github_pat_", 20),
    ("ghp_", 20),
    ("gho_", 20),
    ("ghu_", 20),
    ("ghs_", 20),
    ("ghr_", 20),
    ("glpat-", 16),
    ("sk-ant-", 16),
    ("sk-proj-", 16),
    ("sk-", 32),
    ("xoxb-", 10),
    ("xoxp-", 10),
    ("xoxa-", 10),
    ("xoxs-", 10),
    // A Slack app-level token (D-200).
    ("xapp-", 10),
    ("AKIA", 16),
    ("ASIA", 16),
    ("AIza", 30),
];

/// `s` with what looks like a credential replaced by `***`.
pub fn redact(s: &str) -> String {
    let s = private_keys(s);
    let s = url_credentials(&s);
    let s = bearer(&s);
    let s = key_values(&s);
    token_shapes(&s)
}

/// Whether `s` holds something [`redact`] would take out.
pub fn has_secret(s: &str) -> bool {
    redact(s) != s
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// `-----BEGIN … PRIVATE KEY----- … -----END … PRIVATE KEY-----`.
fn private_keys(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("-----BEGIN ") {
        let header_end = rest[i..]
            .find("-----\n")
            .or_else(|| rest[i + 11..].find("-----").map(|e| e + 11));
        let is_private = rest[i..]
            .lines()
            .next()
            .is_some_and(|l| l.contains("PRIVATE KEY"));
        if !is_private || header_end.is_none() {
            out.push_str(&rest[..i + 11]);
            rest = &rest[i + 11..];
            continue;
        }
        out.push_str(&rest[..i]);
        match rest[i..].find("-----END ") {
            Some(e) => {
                let end = rest[i + e..]
                    .find("KEY-----")
                    .map(|k| i + e + k + "KEY-----".len())
                    .unwrap_or(rest.len());
                out.push_str("-----BEGIN PRIVATE KEY----- *** -----END PRIVATE KEY-----");
                rest = &rest[end..];
            }
            None => {
                out.push_str("-----BEGIN PRIVATE KEY----- ***");
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// `scheme://user:password@host` → `scheme://***@host`.
pub fn url_credentials(s: &str) -> String {
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
                out.push_str(MASK);
                out.push('@');
                rest = &tail[at + 1..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// `Bearer xyz`, `Basic xyz`, `token xyz` after an `Authorization`.
fn bearer(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let lower = rest.to_ascii_lowercase();
        let found = ["bearer ", "basic "]
            .iter()
            .filter_map(|w| {
                lower
                    .match_indices(w)
                    .find(|(i, _)| *i == 0 || !rest.as_bytes()[i - 1].is_ascii_alphanumeric())
            })
            .min_by_key(|(i, _)| *i);
        let Some((i, w)) = found else {
            break;
        };
        let start = i + w.len();
        let value_end = rest[start..]
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
            .map_or(rest.len(), |e| start + e);
        out.push_str(&rest[..start]);
        if value_end > start && rest[start..value_end] != *MASK {
            out.push_str(MASK);
        } else {
            out.push_str(&rest[start..value_end]);
        }
        rest = &rest[value_end..];
    }
    out.push_str(rest);
    out
}

/// `API_TOKEN=xyz`, `password: xyz`, `"client_secret": "xyz"`, `?token=xyz`.
fn key_values(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut last = 0;
    let mut k = 0;
    while k < chars.len() {
        let (pos, c) = chars[k];
        if c != '=' && c != ':' {
            k += 1;
            continue;
        }
        // The key just before: letters, digits, `_`, `-`, `.`, maybe quoted.
        let before = s[..pos].trim_end_matches(['"', '\'']).trim_end();
        // Past the character before the key, which may be more than a byte
        // (`修复这个PR，https://…`, a BOM).
        let key_start = before
            .char_indices()
            .rfind(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let key = before[key_start..].to_ascii_lowercase();
        let secret_key = !key.is_empty()
            && SECRET_KEYS.iter().any(|w| key.ends_with(w) || key.contains(&format!("{w}_")))
            // `https:` and the like are no keys.
            && !(c == ':' && s[pos..].starts_with("://"));
        if !secret_key {
            k += 1;
            continue;
        }
        // The value: after `=`/`:`, spaces and an opening quote.
        let mut v = pos + c.len_utf8();
        let bytes = s.as_bytes();
        while v < s.len() && (bytes[v] == b' ' || bytes[v] == b'\t') {
            v += 1;
        }
        let quote =
            (v < s.len() && (bytes[v] == b'"' || bytes[v] == b'\'')).then(|| bytes[v] as char);
        if quote.is_some() {
            v += 1;
        }
        let end = match quote {
            Some(q) => s[v..].find(q).map_or(s.len(), |e| v + e),
            None => s[v..]
                .find(|c: char| c.is_whitespace() || matches!(c, '&' | ',' | ';' | '"' | '\''))
                .map_or(s.len(), |e| v + e),
        };
        if end > v && &s[v..end] != MASK {
            out.push_str(&s[last..v]);
            out.push_str(MASK);
            last = end;
        }
        while k < chars.len() && chars[k].0 < end.max(pos + 1) {
            k += 1;
        }
    }
    out.push_str(&s[last..]);
    out
}

/// Keys in a known shape: `ghp_…`, `sk-ant-…`, `AKIA…`, …
fn token_shapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'scan: while i < s.len() {
        let boundary = i == 0 || !token_char(s[..i].chars().next_back().unwrap_or(' '));
        if boundary {
            for (prefix, min) in TOKEN_PREFIXES {
                if s[i..].starts_with(prefix) {
                    let body = s[i + prefix.len()..]
                        .find(|c: char| !token_char(c))
                        .unwrap_or(s.len() - i - prefix.len());
                    if body >= *min {
                        out.push_str(prefix);
                        out.push_str(MASK);
                        i += prefix.len() + body;
                        continue 'scan;
                    }
                }
            }
        }
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_their_credentials() {
        assert_eq!(
            redact("fatal: could not read from https://bot:ghx-secret@github.com/o/r.git"),
            "fatal: could not read from https://***@github.com/o/r.git"
        );
        assert_eq!(
            redact("see https://example.com/a@b"),
            "see https://example.com/a@b"
        );
    }

    #[test]
    fn headers_assignments_and_known_shapes_go() {
        for (input, gone) in [
            (
                "curl -H 'Authorization: Bearer abc.def.ghi' x",
                "abc.def.ghi",
            ),
            (
                "export GITHUB_TOKEN=ghp_0123456789abcdefghijABCDEFGHIJ",
                "0123456789abcdefghij",
            ),
            ("DB_PASSWORD = hunter2", "hunter2"),
            (
                r#"{"client_secret": "s3cr3t-value", "id": 1}"#,
                "s3cr3t-value",
            ),
            ("GET /cb?code=1&access_token=zzz&state=2", "zzz"),
            (
                "key: sk-ant-api03-ABCDEFGHIJKLMNOPQRST",
                "ABCDEFGHIJKLMNOPQRST",
            ),
            ("aws AKIAIOSFODNN7EXAMPLE used", "IOSFODNN7EXAMPLE"),
            ("xoxb-1234567890-abcdefghij", "1234567890-abcdefghij"),
            ("xapp-1-A0CBG40MJN6-1234567890", "A0CBG40MJN6-1234567890"),
            (
                // Split so secret scanners don't flag this fake key.
                concat!("api key AIza", "SyA-1234567890abcdefghijklmnopqrstu here"),
                "SyA-1234567890abcdefghij",
            ),
        ] {
            let r = redact(input);
            assert!(!r.contains(gone), "{input} → {r}");
            assert!(r.contains(MASK), "{input} → {r}");
        }
    }

    #[test]
    fn private_keys_go_whole() {
        let pem = "load failed:\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\nAAAA\n-----END OPENSSH PRIVATE KEY-----\ndone";
        let r = redact(pem);
        assert!(!r.contains("b3BlbnNz"), "{r}");
        assert!(
            r.starts_with("load failed:\n-----BEGIN PRIVATE KEY----- ***"),
            "{r}"
        );
        assert!(r.ends_with("\ndone"), "{r}");
        // A certificate isn't a secret.
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        assert_eq!(redact(cert), cert);
    }

    #[test]
    fn ordinary_failures_read_as_they_were() {
        for s in [
            "direnv: error /w/.envrc:3: nix: command not found",
            "make: *** [Makefile:2: test] Error 1",
            "error[E0425]: cannot find value `token` in this scope",
            "cargo test tokenizer::tests -- --nocapture",
            "Authorization required; run `gh auth login`",
            "auth: ok",
            "https://github.com/o/r/pull/7",
            "task-12345 skipped",
        ] {
            assert_eq!(redact(s), s, "{s}");
        }
    }

    #[test]
    fn text_in_any_script_is_read_whole() {
        // Each of these took otterd down: the key's start fell inside the
        // character before it.
        for s in [
            "修复这个PR，https://github.com/o/r/pull/7",
            "\u{feff}Tests: cargo test",
            "看看token=",
            "打开http://localhost:3000",
        ] {
            assert_eq!(redact(s), s, "{s}");
        }
        assert_eq!(redact("密码，TOKEN=abc 好"), "密码，TOKEN=*** 好");
        assert_eq!(redact("\u{feff}password: hunter2"), "\u{feff}password: ***");
    }

    #[test]
    fn redacting_twice_changes_nothing() {
        let s = "TOKEN=abc https://u:p@h/x Bearer zz ghp_0123456789abcdefghijABCD";
        let once = redact(s);
        assert_eq!(redact(&once), once);
        assert!(has_secret(s) && !has_secret(&once));
    }
}
