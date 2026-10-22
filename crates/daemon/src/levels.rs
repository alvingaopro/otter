//! How far each of Otter's tools may go on this host (D-190): the catalog
//! Settings shows, the host's levels, and a task's tightening of them.
//!
//! Every tool is `auto` unless set otherwise, but noting things in Otter's
//! memory: what it keeps comes from talking with the developer, never from
//! what a CI log or a web page says. What no level allows is checked under
//! the tools, not here.

use otter_core::task::Task;
use otter_core::tools::{ToolLevel, effective};
use otter_protocol::host::ToolInfo;

use crate::daemon::Daemon;
use crate::gateway::ToolSpec;

/// A tool in the catalog: its name, its group, what it's for, its default.
pub struct Entry {
    pub name: &'static str,
    pub group: &'static str,
    pub description: &'static str,
    pub default: ToolLevel,
}

const fn e(
    name: &'static str,
    group: &'static str,
    description: &'static str,
    default: ToolLevel,
) -> Entry {
    Entry {
        name,
        group,
        description,
        default,
    }
}

use ToolLevel::{Auto, Developer};

/// Every tool a level is kept for, in the order Settings lists them.
pub const CATALOG: &[Entry] = &[
    // Looking.
    e("get_task", "look", "Read where a task stands", Auto),
    e("get_task_history", "look", "Read a task's history", Auto),
    e("get_run_events", "look", "Read what the coding agent did lately", Auto),
    e("get_pending_decisions", "look", "Read what waits on a decision", Auto),
    e("get_task_evidence", "look", "Read the checks run and their results", Auto),
    e("get_messages", "look", "Read older messages of a conversation", Auto),
    e("list_tasks", "look", "List tasks: the host's, or a task's sub-tasks", Auto),
    e("list_workspaces", "look", "List the host's workspaces", Auto),
    e("list_repositories", "look", "List the repositories on the host", Auto),
    e("read_github", "look", "Read a GitHub Actions run, job or pull request", Auto),
    e("WebSearch", "look", "Search the web", Auto),
    e("WebFetch", "look", "Read a web page", Auto),
    e("Bash", "look", "Run commands in Otter's own empty directory (the command policy applies)", Auto),
    // Where the work happens.
    e("set_workspace", "place", "Choose or change where a task works, before it has worked", Auto),
    e("set_answer_only", "place", "Make a task answer-only, or allow changes again", Auto),
    // The work itself.
    e("start_task", "task", "Start a draft task", Auto),
    e("resume_task", "task", "Resume a paused task", Auto),
    e("retry_task", "task", "Retry a failed task", Auto),
    e("pause_task", "task", "Pause a task", Auto),
    e("revise_task", "task", "Plan a task again from where its work stands", Auto),
    e("approve_plan", "task", "Approve the coding agent's plan", Auto),
    e("accept_task", "task", "Accept a task whose gates all passed", Auto),
    e("finish_with_answer", "task", "End a task Otter answered itself, with nothing to change", Auto),
    e("create_task", "delegate", "Make a task, or a sub-task of the task", Auto),
    e("forward_to_agent", "delegate", "Pass the developer's messages to the coding agent", Auto),
    e("ask_agent", "delegate", "Ask the coding agent the developer's question", Auto),
    e("redirect_agent", "delegate", "Stop the coding agent's turn for the developer's messages", Auto),
    e("tell_agent", "delegate", "Give the coding agent Otter's own instructions", Auto),
    // Coming back to it.
    e("watch", "wait", "Look again later: a pull request's CI, an Actions run, a time", Auto),
    e("cancel_watch", "wait", "Stop a watch", Auto),
    // Delivery.
    e("publish", "deliver", "Push a task's branch and open its pull request", Auto),
    // Memory.
    e("remember", "remember", "Note something in Otter's memory", Developer),
    e("edit_memory", "remember", "Correct Otter's memory", Developer),
];

pub fn entry(name: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|e| e.name == name)
}

/// The SDK's own tools Otter's turn may be given (D-191).
pub const BUILTINS: &[&str] = &["WebSearch", "WebFetch", "Bash"];

impl Daemon {
    /// A tool's level for `task` (the host's, tightened by the task's), or
    /// the host's.
    pub(crate) fn tool_level(&self, task: Option<&Task>, name: &str) -> ToolLevel {
        let host = self.settings.read().unwrap().tool_level(name);
        match task {
            Some(f) => effective(host, &f.tool_levels, name),
            None => host,
        }
    }

    /// The tools a turn is offered: those not off.
    pub(crate) fn offered(&self, task: Option<&Task>, tools: Vec<ToolSpec>) -> Vec<ToolSpec> {
        tools
            .into_iter()
            .filter(|t| self.tool_level(task, t.name) != ToolLevel::Off)
            .collect()
    }

    /// The SDK's own tools a turn is given (none off).
    pub(crate) fn builtins_offered(&self, task: Option<&Task>) -> Vec<&'static str> {
        BUILTINS
            .iter()
            .copied()
            .filter(|t| self.tool_level(task, t) != ToolLevel::Off)
            .collect()
    }

    /// Whether the controller may do what `name` does on its own for
    /// `task` (D-192): its level is `auto`.
    pub(crate) fn auto(&self, task: &Task, name: &str) -> bool {
        self.tool_level(Some(task), name) == ToolLevel::Auto
    }
}

/// The catalog as Settings shows it, with the host's levels.
pub fn view(level: impl Fn(&str) -> ToolLevel) -> Vec<ToolInfo> {
    CATALOG
        .iter()
        .map(|e| ToolInfo {
            name: e.name.into(),
            description: e.description.into(),
            group: e.group.into(),
            level: level(e.name),
            default_level: e.default,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_otter_has_has_a_level() {
        let names: Vec<&str> = crate::gateway::tools()
            .into_iter()
            .chain(crate::gateway::host_tools())
            .map(|t| t.name)
            .chain(BUILTINS.iter().copied())
            .collect();
        for n in &names {
            assert!(entry(n).is_some(), "{n} has no level");
        }
        // And nothing is in the catalog that isn't a tool, but what the
        // controller does on its own.
        for e in CATALOG {
            assert!(
                names.contains(&e.name) || ["publish"].contains(&e.name),
                "{} is no tool",
                e.name
            );
        }
        let mut seen = std::collections::HashSet::new();
        assert!(CATALOG.iter().all(|e| seen.insert(e.name)));
    }

    #[test]
    fn everything_is_automatic_but_memory() {
        for e in CATALOG {
            let want = if e.group == "remember" {
                Developer
            } else {
                Auto
            };
            assert_eq!(e.default, want, "{}", e.name);
        }
    }
}
