//! How far each of Otter's tools may go (D-190): one level per tool, set
//! for the host and tightened for a task, never loosened.
//!
//! The levels say who may start what a tool does: `Auto` — Otter, in any
//! turn, a lifecycle event's too; `Developer` — only a turn the developer's
//! message started; `Off` — Otter isn't offered it at all. What no level
//! allows (merging, force-pushing, deleting outside Otter's own
//! directories, credentials, two writers in one Git tree) is checked under
//! every tool, not here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ToolLevel {
    /// Otter uses it on its own, and says so.
    #[default]
    Auto,
    /// Only on the developer's word: a turn their message started.
    Developer,
    /// Not offered.
    Off,
}

impl ToolLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolLevel::Auto => "auto",
            ToolLevel::Developer => "developer",
            ToolLevel::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<ToolLevel> {
        match s {
            "auto" => Some(ToolLevel::Auto),
            "developer" => Some(ToolLevel::Developer),
            "off" => Some(ToolLevel::Off),
            _ => None,
        }
    }

    fn rank(self) -> u8 {
        match self {
            ToolLevel::Auto => 0,
            ToolLevel::Developer => 1,
            ToolLevel::Off => 2,
        }
    }

    /// The stricter of the two.
    pub fn stricter(self, other: ToolLevel) -> ToolLevel {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    /// Whether a turn may use the tool: `authority` is whether the
    /// developer's message started it.
    pub fn allows(self, authority: bool) -> bool {
        match self {
            ToolLevel::Auto => true,
            ToolLevel::Developer => authority,
            ToolLevel::Off => false,
        }
    }
}

/// A task's own levels over the host's: each the stricter (a task tightens,
/// never loosens).
pub fn effective(host: ToolLevel, task: &BTreeMap<String, ToolLevel>, tool: &str) -> ToolLevel {
    match task.get(tool) {
        Some(t) => host.stricter(*t),
        None => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_tightens_and_never_loosens() {
        let mut task = BTreeMap::new();
        task.insert("Bash".to_owned(), ToolLevel::Off);
        task.insert("watch".to_owned(), ToolLevel::Auto);
        assert_eq!(effective(ToolLevel::Auto, &task, "Bash"), ToolLevel::Off);
        assert_eq!(
            effective(ToolLevel::Developer, &task, "watch"),
            ToolLevel::Developer
        );
        assert_eq!(
            effective(ToolLevel::Developer, &task, "publish"),
            ToolLevel::Developer
        );
    }

    #[test]
    fn levels_say_who_may_start_a_tool() {
        assert!(ToolLevel::Auto.allows(false));
        assert!(!ToolLevel::Developer.allows(false));
        assert!(ToolLevel::Developer.allows(true));
        assert!(!ToolLevel::Off.allows(true));
        for l in [ToolLevel::Auto, ToolLevel::Developer, ToolLevel::Off] {
            assert_eq!(ToolLevel::parse(l.as_str()), Some(l));
            assert_eq!(serde_json::to_value(l).unwrap(), l.as_str());
        }
    }
}
