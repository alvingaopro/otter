//! Slack as Otter uses it (D-200): what Slack sends down Socket Mode
//! (`socket`), and its Web API through curl (`api`). otterd says Otter's
//! messages in Slack with them; `otter hub` holds one app's socket for
//! several hosts and passes each event to the host it is for (D-206).

pub mod api;
pub mod socket;

/// The environment curl runs with, as otterd's environments give it.
pub type EnvMap = std::collections::BTreeMap<String, String>;

/// Slack members as Settings takes them: member IDs (`U…`, `W…`), each once.
pub fn members(users: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for u in users.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
        let id = u.trim_start_matches('@');
        let ok = id.len() >= 3
            && id.len() <= 32
            && (id.starts_with('U') || id.starts_with('W'))
            && id
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if !ok {
            return Err(format!(
                "`{u}` isn't a Slack member ID (U…: in Slack, your profile › ⋯ › Copy member ID)"
            ));
        }
        if !out.iter().any(|o| o == id) {
            out.push(id.to_owned());
        }
    }
    if out.len() > 20 {
        return Err("at most 20 Slack members".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_ids_are_taken_as_slack_writes_them() {
        let ids = |v: &[&str]| members(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            ids(&[" U06MPBPRX7C ", "@W012ABC", "U06MPBPRX7C", ""]).unwrap(),
            vec!["U06MPBPRX7C", "W012ABC"]
        );
        for bad in ["alvin", "u06mpbprx7c", "C123", "U1-2"] {
            let err = ids(&[bad]).unwrap_err();
            assert!(err.contains("Copy member ID"), "{bad}: {err}");
        }
        assert!(ids(&[]).unwrap().is_empty());
    }
}
