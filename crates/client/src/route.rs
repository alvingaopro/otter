//! Which host a request is about (D-179).
//!
//! otterd knows one host, so the choice of host is the client's, made before
//! Otter's conversation on it. From the workspaces each host has, a message
//! that names one of them — by name, or by the repository it comes from —
//! belongs to the host that has it. No model is asked: this is string
//! matching, and a wrong guess costs a switch of host, not a task.
//!
//! The app has the same rules in `apps/desktop/src/route.ts`; change both.

use otter_core::{Workspace, WorkspaceSource};

/// What a message said about a host's workspaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub host: String,
    /// What matched, for the developer to read: "workspace `billing`".
    pub why: String,
    score: u8,
}

/// Where a message goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// One host has what the message names.
    One(Match),
    /// Several do, equally: the developer chooses.
    Several(Vec<Match>),
    /// None does (a repository nobody has yet, or no repository at all).
    Nowhere,
}

const STRONG: u8 = 2;
const WEAK: u8 = 1;
/// Shortest name taken for a word on its own: shorter ones (`api`, `web`,
/// `add`) are in every sentence.
const MIN_NAME: usize = 4;
const MIN_REPOSITORY_WORD: usize = 4;

/// Route `text` among `hosts`, each with the workspaces it has.
pub fn route(text: &str, hosts: &[(String, Vec<Workspace>)]) -> Route {
    let text = text.to_lowercase();
    let mut found: Vec<Match> = hosts
        .iter()
        .filter_map(|(host, workspaces)| {
            workspaces
                .iter()
                .filter_map(|w| matched(&text, w))
                .max_by_key(|(score, _)| *score)
                .map(|(score, why)| Match {
                    host: host.clone(),
                    why,
                    score,
                })
        })
        .collect();
    let Some(best) = found.iter().map(|m| m.score).max() else {
        return Route::Nowhere;
    };
    found.retain(|m| m.score == best);
    match found.len() {
        1 => Route::One(found.remove(0)),
        _ => Route::Several(found),
    }
}

fn matched(text: &str, w: &Workspace) -> Option<(u8, String)> {
    let name = w.name.to_lowercase();
    if name.chars().count() >= MIN_NAME && contains_word(text, &name) {
        return Some((STRONG, format!("workspace `{}`", w.name)));
    }
    let WorkspaceSource::Git(g) = &w.source else {
        return None;
    };
    let (owner, repo) = identity(&g.repository)?;
    if let Some(owner) = &owner
        && contains_word(text, &format!("{owner}/{repo}"))
    {
        return Some((STRONG, format!("repository {owner}/{repo}")));
    }
    if repo.chars().count() >= MIN_REPOSITORY_WORD && contains_word(text, &repo) {
        return Some((WEAK, format!("repository {repo}")));
    }
    None
}

/// `(owner, name)` of a repository address, lowercased: `acme/billing` for
/// `git@github.com:acme/billing.git` and `https://token@github.com/acme/billing/`.
fn identity(repository: &str) -> Option<(Option<String>, String)> {
    let trimmed = repository.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut parts = trimmed.rsplit(['/', ':']).filter(|s| !s.is_empty());
    let name = parts.next()?.to_lowercase();
    let owner = parts
        .next()
        .filter(|o| !o.contains('@') && !o.contains('.'))
        .map(str::to_lowercase);
    Some((owner, name))
}

/// `needle` in `hay` as a word: not inside a longer name. Names are ASCII;
/// any other character (a Chinese or Japanese sentence has no spaces) ends one.
fn contains_word(hay: &str, needle: &str) -> bool {
    let name_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    hay.match_indices(needle).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + needle.len()..].chars().next();
        !before.is_some_and(name_char) && !after.is_some_and(name_char)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::{GitSource, WorkspaceState};

    fn ws(name: &str, repository: Option<&str>) -> Workspace {
        let now = chrono::Utc::now();
        Workspace {
            id: format!("ws_{name}").as_str().into(),
            name: name.into(),
            root: format!("/w/{name}"),
            brief: Default::default(),
            source: repository.map_or(WorkspaceSource::Empty, |r| {
                WorkspaceSource::Git(GitSource {
                    repository: r.into(),
                    repo_id: String::new(),
                    branch: String::new(),
                    base: None,
                    base_commit: None,
                    requested_branch: None,
                })
            }),
            environment: Default::default(),
            state: WorkspaceState::Ready,
            state_message: None,
            created_at: now,
            updated_at: now,
            sessions: vec![],
            attention: vec![],
            task_id: None,
        }
    }

    fn hosts() -> Vec<(String, Vec<Workspace>)> {
        vec![
            (
                "dev".into(),
                vec![
                    ws("billing", Some("git@github.com:acme/billing.git")),
                    ws("web", Some("https://tok@github.com/acme/web-app")),
                ],
            ),
            (
                "gpu".into(),
                vec![ws("trainer", Some("git@github.com:acme/trainer.git"))],
            ),
        ]
    }

    fn one(r: Route) -> (String, String) {
        match r {
            Route::One(m) => (m.host, m.why),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_workspace_named_goes_to_its_host() {
        assert_eq!(
            one(route("Add retries in billing, please.", &hosts())),
            ("dev".into(), "workspace `billing`".into())
        );
    }

    #[test]
    fn a_repository_named_goes_to_the_host_that_has_it() {
        assert_eq!(one(route("train faster: acme/trainer", &hosts())).0, "gpu");
        assert_eq!(
            one(route(
                "fix https://github.com/acme/trainer.git slowness",
                &hosts()
            ))
            .0,
            "gpu"
        );
    }

    #[test]
    fn the_repository_name_alone_is_enough_but_weaker() {
        let (host, why) = one(route("something in trainer", &hosts()));
        assert_eq!(
            (host.as_str(), why.as_str()),
            ("gpu", "workspace `trainer`")
        );
        // Matches a repository, not a workspace's name, on one host only.
        let (host, why) = one(route("the web-app login is slow", &hosts()));
        assert_eq!((host.as_str(), why.as_str()), ("dev", "repository web-app"));
    }

    #[test]
    fn a_short_name_is_not_taken_from_a_sentence() {
        let hosts = vec![("a".into(), vec![ws("api", None), ws("web", None)])];
        assert_eq!(
            route("add an api endpoint to the web page", &hosts),
            Route::Nowhere
        );
    }

    #[test]
    fn a_name_in_a_sentence_without_spaces_is_found() {
        let hosts = vec![("a".into(), vec![ws("billing", None)])];
        assert_eq!(one(route("在billing里加上重试", &hosts)).0, "a");
        assert_eq!(route("在billing2里加上重试", &hosts), Route::Nowhere);
    }

    #[test]
    fn names_are_measured_in_characters_and_found_whole() {
        let hosts = vec![("a".into(), vec![ws("日本語アプリ", None)])];
        assert_eq!(one(route("日本語アプリを直して", &hosts)).0, "a");
        assert_eq!(route("x日本語アプリ", &hosts), Route::Nowhere);
        // Three characters, nine bytes: too short either way.
        let hosts = vec![("a".into(), vec![ws("日本語", None)])];
        assert_eq!(route("日本語", &hosts), Route::Nowhere);
    }

    #[test]
    fn a_longer_name_does_not_contain_a_shorter_one() {
        assert_eq!(
            route("rebuild the billing-engine", &hosts()),
            Route::Nowhere
        );
        assert_eq!(route("subbilling", &hosts()), Route::Nowhere);
    }

    #[test]
    fn nothing_named_is_nowhere_and_equals_are_several() {
        assert_eq!(route("write a csv converter", &hosts()), Route::Nowhere);
        let both = vec![
            ("a".into(), vec![ws("billing", None)]),
            ("b".into(), vec![ws("billing", None)]),
        ];
        let Route::Several(m) = route("billing", &both) else {
            panic!()
        };
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn a_stronger_match_beats_a_weaker_one() {
        let hosts = vec![
            (
                "a".into(),
                vec![ws("misc", Some("git@h:other/trainer.git"))],
            ),
            ("b".into(), vec![ws("trainer", None)]),
        ];
        assert_eq!(one(route("trainer", &hosts)).0, "b");
    }

    #[test]
    fn credentials_in_a_stored_address_are_not_a_name() {
        let hosts = vec![(
            "a".into(),
            vec![ws(
                "x1",
                Some("https://secret-token@github.com/acme/billing.git"),
            )],
        )];
        assert_eq!(route("secret-token", &hosts), Route::Nowhere);
        assert_eq!(one(route("acme/billing", &hosts)).0, "a");
    }
}
