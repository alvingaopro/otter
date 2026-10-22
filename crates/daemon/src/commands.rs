//! The developer's slash commands in a task's conversation (D-190). A
//! message that says what to do is done by otterd with the tool Otter would
//! have chosen for it — the same gateway, the same checks, applied once,
//! written to the task's history — and a line of Otter's says what came of
//! it. Otter's turn doesn't answer such a message (`controller::unanswered`):
//! nothing is left to a model's reading of it.

use std::sync::Arc;

use otter_core::TaskId;
use otter_core::task::{Message, MessageCommand, MessageRole, PICTURES_ONLY, Task};
use serde_json::{Value, json};

use crate::daemon::{Daemon, RpcResult};
use crate::gateway::{Gateway, Origin, Scope};
use crate::tasks::message;

/// What Otter's line about a command is filed under. Not the message's own
/// id: a reply to `msg_…` answers every message before it (`unanswered`).
fn said_key(m: &Message) -> String {
    format!("command:{}", m.id)
}

/// The developer's commands not done yet, oldest first.
pub(crate) fn pending(f: &Task) -> Vec<&Message> {
    f.messages
        .iter()
        .filter(|m| m.role == MessageRole::User && m.command.is_some())
        .filter(|m| {
            let key = said_key(m);
            !f.messages
                .iter()
                .any(|o| o.correlation_id.as_deref() == Some(key.as_str()))
        })
        .collect()
}

/// The tool a command is done with, and what it is given.
fn tool(c: &MessageCommand, m: &Message) -> (&'static str, Value) {
    let words = c.words(&m.text);
    let what = |default: &str| {
        if words.is_empty() {
            default.to_owned()
        } else {
            words.to_owned()
        }
    };
    match c {
        MessageCommand::Ask => ("ask_agent", json!({"message_ids": [m.id]})),
        MessageCommand::Tell => ("forward_to_agent", json!({"message_ids": [m.id]})),
        MessageCommand::Revise => ("revise_task", json!({})),
        MessageCommand::Remember => ("remember", json!({"memory": "conversation", "note": words})),
        MessageCommand::WatchCi => ("watch", json!({"kind": "ci", "what": what("its CI")})),
        MessageCommand::Remind { in_minutes } => (
            "watch",
            json!({"kind": "time", "in_minutes": in_minutes, "what": what("look at this task again")}),
        ),
        MessageCommand::AnswerOnly { on } => ("set_answer_only", json!({"on": on})),
    }
}

/// Otter's line about what a command came to.
fn said(c: &MessageCommand, answer: &Value) -> String {
    if answer["ok"] != true {
        let e = &answer["error"];
        let code = e["code"].as_str().unwrap_or_default();
        let doing = match c {
            MessageCommand::Ask => "ask the coding agent",
            MessageCommand::Tell => "pass that on",
            MessageCommand::Revise => "plan again",
            MessageCommand::Remember => "note that",
            MessageCommand::WatchCi => "watch CI",
            MessageCommand::Remind { .. } => "set that reminder",
            MessageCommand::AnswerOnly { .. } => "change that",
        };
        let why = e["message"].as_str().unwrap_or("it was refused");
        // The tools' own next steps are written for Otter; these are for
        // the developer.
        let next = match (c, code) {
            (MessageCommand::Ask, "not_ready") => {
                Some("Ask while it works, or start a task for the question.")
            }
            (
                MessageCommand::Ask | MessageCommand::AnswerOnly { .. },
                "not_allowed_in_this_turn",
            ) => Some("In Settings, run the coding agent through the Claude Agent SDK worker."),
            (MessageCommand::Remember, "limit") => {
                Some("Make room in Memory, in the task's menu, and say it again.")
            }
            _ => None,
        };
        return match next {
            Some(next) => format!("I couldn't {doing}: {why}. {next}"),
            None => format!("I couldn't {doing}: {why}."),
        };
    }
    let d = &answer["data"];
    match c {
        MessageCommand::Ask => {
            "Asked the coding agent. It only reads for this; its answer comes here.".into()
        }
        MessageCommand::Tell => {
            "Passed on to the coding agent as you wrote it, after its current turn.".into()
        }
        MessageCommand::Revise => "Planning again from where the work is.".into(),
        MessageCommand::Remember if d["done"] == "already noted" => {
            "That's in this task's memory already.".into()
        }
        MessageCommand::Remember => "Noted in this task's memory.".into(),
        MessageCommand::WatchCi => watching(d),
        MessageCommand::Remind { in_minutes } => {
            format!("I'll look at this again in {}.", minutes(*in_minutes))
        }
        MessageCommand::AnswerOnly { on: true } => {
            "Only an answer from now on: the coding agent only reads.".into()
        }
        MessageCommand::AnswerOnly { on: false } => {
            "Changes are fine again: the task is no longer answer-only.".into()
        }
    }
}

/// What a CI watch read as it was set (D-188): what it is now, or why it
/// couldn't read, or that there was nothing to wait for.
fn watching(d: &Value) -> String {
    let text = |v: &Value| {
        v.as_str()
            .unwrap_or_default()
            .trim_end_matches('.')
            .to_owned()
    };
    if d["done"] == "nothing to wait for" {
        return format!("Nothing to wait for: {}.", text(&d["came_to"]["text"]));
    }
    let label = text(&d["watching"]);
    let mut line = if d["done"] == "watching" {
        format!("Watching {label} until it finishes; I'll say here what it comes to.")
    } else {
        format!("Already watching {label}.")
    };
    if !d["now"]["text"].is_null() {
        line.push_str(&format!(" Just read: {}.", text(&d["now"]["text"])));
    } else if !d["not_read"].is_null() {
        line.push_str(&format!(
            " I couldn't read it yet ({}); I'll keep trying.",
            text(&d["not_read"])
        ));
    }
    line
}

/// `45 min`, `2 h`, `1 h 30 min`.
fn minutes(n: u32) -> String {
    match (n / 60, n % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

impl Daemon {
    /// Do the developer's commands not done yet (D-190), oldest first. Each
    /// is done once whatever happens to otterd in between: the tool's change
    /// is applied under the message, and Otter's line under the command.
    pub(crate) async fn run_commands(self: &Arc<Self>, id: &TaskId) -> RpcResult<()> {
        let f = self.task_get(id.as_str()).await?;
        let todo: Vec<Message> = pending(&f).into_iter().cloned().collect();
        for m in todo {
            let Some(c) = m.command.clone() else { continue };
            let (name, args) = tool(&c, &m);
            // What the tool passes on is the words, not the command: the
            // coding agent isn't told Otter's syntax.
            let words = c.words(&m.text);
            let asked = Message {
                text: if words.is_empty() {
                    PICTURES_ONLY.to_owned()
                } else {
                    words.to_owned()
                },
                ..m.clone()
            };
            let gateway = Gateway::restricted(
                self.clone(),
                Scope::Task(id.clone()),
                Origin::Developer {
                    messages: vec![asked],
                },
                1,
                vec![name],
            );
            let answer = gateway.call(name, args).await;
            let line = said(&c, &answer);
            let key = said_key(&m);
            let applied = self.tasks.lock().await.apply(id, Some(&key), |f, _| {
                let (reply, change) = message(MessageRole::Controller, line, Some(key.clone()));
                f.messages.push(reply);
                Ok(vec![change])
            })?;
            self.task_changed(&applied);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_written_as_typed_and_read_back() {
        let c = MessageCommand::Remind { in_minutes: 90 };
        assert_eq!(c.line("check the deploy"), "/remind 1h30m check the deploy");
        assert_eq!(
            c.words("/remind 1h30m check the deploy"),
            "check the deploy"
        );
        assert_eq!(MessageCommand::WatchCi.line("  "), "/watch ci");
        assert_eq!(MessageCommand::WatchCi.words("/watch ci"), "");
        assert_eq!(
            MessageCommand::AnswerOnly { on: false }.line(""),
            "/answer-only off"
        );
        assert_eq!(
            MessageCommand::Ask.words("/ask where is it?"),
            "where is it?"
        );
    }

    #[test]
    fn a_refusal_says_why_and_what_the_developer_can_do() {
        let refused = json!({"ok": false, "error": {
            "code": "not_ready",
            "message": "the coding agent isn't at work on this task",
            "next": "a question on its own is answered as a task of its own: the developer can start one",
        }});
        assert_eq!(
            said(&MessageCommand::Ask, &refused),
            "I couldn't ask the coding agent: the coding agent isn't at work on this task. Ask while it works, or start a task for the question."
        );
        // Otter's next step isn't the developer's.
        let full = json!({"ok": false, "error": {
            "code": "conflict", "message": "this task already has 3 watches open (a; b; c): stop one first",
            "next": "tell the developer",
        }});
        assert_eq!(
            said(&MessageCommand::Remind { in_minutes: 30 }, &full),
            "I couldn't set that reminder: this task already has 3 watches open (a; b; c): stop one first."
        );
    }

    #[test]
    fn a_ci_watch_says_what_it_read() {
        let set = json!({"ok": true, "data": {
            "done": "watching", "watching": "CI on #182",
            "now": {"text": "3 of 5 checks running.", "read_at": "2026-10-22T14:02:00Z"},
        }});
        assert_eq!(
            said(&MessageCommand::WatchCi, &set),
            "Watching CI on #182 until it finishes; I'll say here what it comes to. Just read: 3 of 5 checks running."
        );
        let unread = json!({"ok": true, "data": {
            "done": "watching", "watching": "CI on #182", "now": null, "not_read": "gh isn't signed in",
        }});
        assert!(
            said(&MessageCommand::WatchCi, &unread)
                .ends_with("I couldn't read it yet (gh isn't signed in); I'll keep trying.")
        );
        let over = json!({"ok": true, "data": {
            "done": "nothing to wait for", "watching": "CI on #182",
            "came_to": {"outcome": "passed", "text": "every check passed"},
        }});
        assert_eq!(
            said(&MessageCommand::WatchCi, &over),
            "Nothing to wait for: every check passed."
        );
    }

    #[test]
    fn only_a_command_without_otters_line_is_pending() {
        let now = chrono::Utc::now();
        let msg = |id: &str, role, command, correlation: Option<&str>| Message {
            id: otter_core::MessageId::from(id),
            role,
            text: "x".into(),
            at: now,
            correlation_id: correlation.map(Into::into),
            attachments: vec![],
            command,
        };
        let mut f = Task::new("t".into(), String::new(), now);
        f.messages = vec![
            msg("msg_a", MessageRole::User, None, None),
            msg("msg_b", MessageRole::User, Some(MessageCommand::Ask), None),
            msg("msg_c", MessageRole::User, Some(MessageCommand::Tell), None),
            msg(
                "msg_d",
                MessageRole::Controller,
                None,
                Some("command:msg_b"),
            ),
        ];
        let ids: Vec<&str> = pending(&f).iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["msg_c"]);
    }
}
