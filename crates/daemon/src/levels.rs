//! How far each of Otter's tools may go on this host (D-191): the catalog
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
    e(
        "get_run_events",
        "look",
        "Read what the coding agent did lately",
        Auto,
    ),
    e(
        "get_pending_decisions",
        "look",
        "Read what waits on a decision",
        Auto,
    ),
    e(
        "get_task_evidence",
        "look",
        "Read the checks run and their results",
        Auto,
    ),
    e(
        "get_messages",
        "look",
        "Read older messages of a conversation",
        Auto,
    ),
    e(
        "list_tasks",
        "look",
        "List tasks: the host's, or a task's sub-tasks",
        Auto,
    ),
    e(
        "list_workspaces",
        "look",
        "List the host's workspaces",
        Auto,
    ),
    e(
        "list_repositories",
        "look",
        "List the repositories on the host",
        Auto,
    ),
    e(
        "read_github",
        "look",
        "Read a GitHub Actions run, job or pull request",
        Auto,
    ),
    e("WebSearch", "look", "Search the web", Auto),
    e("WebFetch", "look", "Read a web page", Auto),
    e(
        "Bash",
        "look",
        "Run commands in Otter's own empty directory (the command policy applies)",
        Auto,
    ),
    // Where the work happens.
    e(
        "set_workspace",
        "place",
        "Choose or change where a task works, before it has worked",
        Auto,
    ),
    e(
        "set_answer_only",
        "place",
        "Make a task answer-only, or allow changes again",
        Auto,
    ),
    // The work itself.
    e("start_task", "task", "Start a draft task", Auto),
    e("resume_task", "task", "Resume a paused task", Auto),
    e("retry_task", "task", "Retry a failed task", Auto),
    e("pause_task", "task", "Pause a task", Auto),
    e(
        "revise_task",
        "task",
        "Plan a task again from where its work stands",
        Auto,
    ),
    e(
        "approve_plan",
        "task",
        "Approve the coding agent's plan",
        Auto,
    ),
    e(
        "finish_with_answer",
        "task",
        "End a task Otter answered itself, with nothing to change",
        Auto,
    ),
    // Only ever on the developer's word, whatever the level (D-205).
    e(
        "answer_decision",
        "task",
        "Answer what a task asks the developer, as they said",
        Developer,
    ),
    e(
        "create_task",
        "delegate",
        "Make a task, or a sub-task of the task",
        Auto,
    ),
    e(
        "forward_to_agent",
        "delegate",
        "Pass the developer's messages to the coding agent",
        Auto,
    ),
    e(
        "ask_agent",
        "delegate",
        "Ask the coding agent the developer's question",
        Auto,
    ),
    e(
        "redirect_agent",
        "delegate",
        "Stop the coding agent's turn for the developer's messages",
        Auto,
    ),
    e(
        "tell_agent",
        "delegate",
        "Give the coding agent Otter's own instructions",
        Auto,
    ),
    // Coming back to it.
    e(
        "watch",
        "wait",
        "Look again later: a pull request's CI, an Actions run, a time",
        Auto,
    ),
    e("cancel_watch", "wait", "Stop a watch", Auto),
    // Delivery.
    e(
        "publish",
        "deliver",
        "Push a task's branch and open its pull request",
        Auto,
    ),
    e(
        "review_pull_request",
        "deliver",
        "Approve, comment on or ask for changes on a pull request",
        Developer,
    ),
    // Memory.
    e(
        "remember",
        "remember",
        "Note something in Otter's memory",
        Developer,
    ),
    e(
        "edit_memory",
        "remember",
        "Correct Otter's memory",
        Developer,
    ),
];

pub fn entry(name: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|e| e.name == name)
}

/// The SDK's own tools Otter's turn may be given (D-192).
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
    /// `task` (D-193): its level is `auto`.
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

impl Daemon {
    /// What Otter did across the host's tasks (D-193), newest first: each
    /// change it set off (`ToolCalled`), with what came of it and what
    /// undoes it, from the tasks' histories. The tasks touched last first;
    /// at most `limit`.
    pub(crate) async fn otter_actions(
        &self,
        p: otter_protocol::otter::OtterActions,
    ) -> crate::daemon::RpcResult<Vec<otter_protocol::otter::OtterAction>> {
        use otter_core::task::TaskEvent;
        use otter_protocol::otter::{OtterAction, Undo};
        let limit = p.limit.unwrap_or(100).clamp(1, 500) as usize;
        // What to read, taken at once; the histories are read without the
        // store's lock, off the async threads.
        let (dir, mut tasks) = {
            let store = self.tasks.lock().await;
            let tasks: Vec<(otter_core::TaskId, String, otter_core::Timestamp)> = store
                .list()
                .into_iter()
                .map(|f| (f.id.clone(), f.title.clone(), f.updated_at))
                .collect();
            (store.dir().to_path_buf(), tasks)
        };
        tasks.sort_by_key(|(_, _, at)| std::cmp::Reverse(*at));
        tasks.truncate(200);
        let all = tokio::task::spawn_blocking(move || {
            let mut all = Vec::new();
            for (id, title, _) in &tasks {
                let history = match crate::tasks::read_history(&dir, id, 0, usize::MAX) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!(task = %id, "reading the history: {e:#}");
                        continue;
                    }
                };
                for (i, rec) in history.iter().enumerate() {
                    let TaskEvent::ToolCalled {
                        tool,
                        origin,
                        authority,
                        detail,
                    } = &rec.event
                    else {
                        continue;
                    };
                    // What came of it — its first `ToolDone` (a later one is
                    // a refusal of its own) — and what undoes it, before the
                    // next call.
                    let mut error = None;
                    let mut done = false;
                    let mut undo = None;
                    for later in &history[i + 1..] {
                        match &later.event {
                            TaskEvent::ToolCalled { .. } => break,
                            TaskEvent::ToolDone { tool: t, error: e } if t == tool => {
                                if done {
                                    break;
                                }
                                done = true;
                                error = e.clone();
                            }
                            TaskEvent::ToolUndo {
                                tool: t,
                                task_id,
                                action,
                            } if t == tool => {
                                undo = Some(Undo {
                                    task_id: task_id.clone(),
                                    action: action.clone(),
                                });
                            }
                            _ => {}
                        }
                    }
                    all.push(OtterAction {
                        task_id: id.clone(),
                        task_title: otter_core::redact::redact(title),
                        seq: rec.seq,
                        at: rec.ts,
                        tool: tool.clone(),
                        origin: origin.clone(),
                        authority: authority.clone(),
                        detail: detail.as_deref().map(otter_core::redact::redact),
                        error,
                        undo,
                    });
                }
            }
            all
        })
        .await
        .map_err(|e| otter_protocol::RpcError::internal(format!("reading what Otter did: {e}")))?;
        let mut all = all;
        all.sort_by_key(|a| std::cmp::Reverse(a.at));
        all.truncate(limit);
        Ok(all)
    }
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
    fn everything_is_automatic_but_memory_decisions_and_reviews() {
        for e in CATALOG {
            // A decision answered and a pull request reviewed are the
            // developer's word (D-205).
            let want = if e.group == "remember"
                || ["answer_decision", "review_pull_request"].contains(&e.name)
            {
                Developer
            } else {
                Auto
            };
            assert_eq!(e.default, want, "{}", e.name);
        }
    }
}
