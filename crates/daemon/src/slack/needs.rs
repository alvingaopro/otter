//! What Slack is told when a task needs the developer (D-201), and what
//! came of it: pure functions over the task, as the app's decision card
//! has them (options, else Approve and Deny; Retry for a task that stopped).

use std::collections::HashMap;

use otter_core::DecisionId;
use otter_core::Timestamp;
use otter_core::conversation::Question;
use otter_core::model::Activity;
use otter_core::task::{Decider, DecisionKind, DecisionStatus, Need, NeedKind, Task, TaskStatus};
use serde_json::{Value, json};

/// Buttons for a question's options, at most.
const MAX_OPTIONS: usize = 10;
/// What a button's label may hold (Slack's limit).
const MAX_LABEL: usize = 75;
/// What a message tells, at most: Slack takes 12 000 characters of Markdown
/// in a message, and an update adds a line.
const MAX_TOLD: usize = 6_000;
/// The whole command a decision is about, at most (D-157 bounds it too).
const MAX_COMMAND: usize = 2_500;

/// A need, as one: the same need again is the same key (D-073's rule).
pub fn key(n: &Need) -> String {
    let kind = match n.kind {
        NeedKind::Decision => "decision",
        NeedKind::Question => "question",
        NeedKind::Review => "review",
        NeedKind::Blocked => "blocked",
        NeedKind::Failed => "failed",
    };
    match &n.decision_id {
        Some(d) => format!("{kind}:{d}"),
        None => format!("{kind}:{}", n.since.timestamp_millis()),
    }
}

/// What a need's message says (also what an update keeps), and its blocks.
pub struct Told {
    pub text: String,
    pub blocks: Value,
}

/// Questions in a form, at most: Slack takes 50 blocks in a message.
const MAX_QUESTIONS: usize = 10;
/// What a select option's value may hold (Slack's limit).
const MAX_VALUE: usize = 150;

/// The message that tells the developer `task` needs them. `form`: the
/// questions of a decision asked as a form (several questions, or choices
/// to pick several of, D-202).
pub fn told(task: &Task, n: &Need, form: Option<&[Question]>) -> Told {
    let d = n
        .decision_id
        .as_ref()
        .and_then(|id| task.decisions.iter().find(|d| &d.id == id));
    let title = &task.title;
    let summary = &n.summary;
    let mut text = match n.kind {
        NeedKind::Decision => format!("**{title}** asks you to decide: {summary}"),
        NeedKind::Question => format!("**{title}** asks: {summary}"),
        NeedKind::Review => format!("**{title}** is ready for your review: {summary}"),
        NeedKind::Blocked => format!("**{title}** is waiting on you: {summary}"),
        NeedKind::Failed => format!("**{title}** stopped: {summary}"),
    };
    if let Some(command) = d.and_then(|d| d.command.as_deref()) {
        text.push_str(&format!("\n```\n{}\n```", cut(command, MAX_COMMAND)));
    }
    if n.decisions > 1 {
        let more = n.decisions - 1;
        let what = if more == 1 { "decision" } else { "decisions" };
        text.push_str(&format!("\n_{more} more {what} after this one._"));
    }
    // Summaries and commands are redacted where they are made, a status's
    // reason not always: again here (invariant 13).
    let text = cut(&otter_core::redact::redact(&text), MAX_TOLD);
    let value = |answer: Option<&str>| {
        let mut v = json!({ "task": task.id.as_str() });
        if let Some(d) = d {
            v["decision"] = json!(d.id.as_str());
        }
        if let Some(a) = answer {
            v["answer"] = json!(a);
        }
        v.to_string()
    };
    let button = |label: &str, action: &str, value: String, style: Option<&str>| {
        let mut b = json!({
            "type": "button",
            "text": { "type": "plain_text", "text": cut(label, MAX_LABEL) },
            "action_id": action,
            "value": value,
        });
        if let Some(s) = style {
            b["style"] = json!(s);
        }
        b
    };
    if let (NeedKind::Question, Some(questions)) = (n.kind, form) {
        return asked(&text, questions, &value(None));
    }
    let mut buttons = Vec::new();
    let hint = match (n.kind, d) {
        (NeedKind::Question, Some(d)) if !d.options.is_empty() => {
            for (i, o) in d.options.iter().take(MAX_OPTIONS).enumerate() {
                let o = otter_core::redact::redact(o);
                buttons.push(button(&o, &format!("answer:{i}"), value(Some(&o)), None));
            }
            "Or reply in this thread to tell the task."
        }
        (NeedKind::Question, _) => "Reply in this thread to answer.",
        (NeedKind::Decision, Some(_)) => {
            buttons.push(button("Approve", "approve", value(None), Some("primary")));
            buttons.push(button("Deny", "deny", value(None), Some("danger")));
            "Or reply in this thread to tell the task."
        }
        (NeedKind::Failed, _) => {
            buttons.push(button("Retry", "retry", value(None), Some("primary")));
            "Or reply in this thread to tell the task what to change."
        }
        _ => "Reply in this thread to tell the task.",
    };
    let mut blocks = vec![json!({ "type": "markdown", "text": format!("{text}\n\n_{hint}_") })];
    if !buttons.is_empty() {
        blocks.push(json!({ "type": "actions", "elements": buttons }));
    }
    Told {
        text,
        blocks: Value::Array(blocks),
    }
}

/// A form in Slack (D-202), as the app's: each question's choices in a
/// select (several where it takes several), a box for one with none, and
/// Submit.
fn asked(text: &str, questions: &[Question], value: &str) -> Told {
    let plain = |t: &str| json!({ "type": "plain_text", "text": cut(t, MAX_LABEL) });
    let mut blocks = vec![json!({ "type": "markdown", "text": text })];
    for (i, q) in questions.iter().take(MAX_QUESTIONS).enumerate() {
        let head = match &q.header {
            Some(h) => format!("**{}** {}", h, q.prompt),
            None => format!("**{}.** {}", i + 1, q.prompt),
        };
        blocks.push(json!({ "type": "markdown", "text": otter_core::redact::redact(&head) }));
        let block_id = format!("q:{}", q.id);
        if q.options.is_empty() {
            blocks.push(json!({
                "type": "input",
                "block_id": block_id,
                "label": plain("Your answer"),
                "element": { "type": "plain_text_input", "action_id": "text" },
            }));
            continue;
        }
        let options: Vec<Value> = q
            .options
            .iter()
            .take(100)
            .map(|o| {
                let label = otter_core::redact::redact(&o.label);
                json!({ "text": plain(&label), "value": cut(&label, MAX_VALUE) })
            })
            .collect();
        let kind = if q.multi_select {
            "multi_static_select"
        } else {
            "static_select"
        };
        let placeholder = if q.multi_select {
            "Choose any"
        } else {
            "Choose one"
        };
        blocks.push(json!({
            "type": "actions",
            "block_id": block_id,
            "elements": [{
                "type": kind,
                "action_id": "pick",
                "placeholder": plain(placeholder),
                "options": options,
            }],
        }));
    }
    blocks.push(json!({
        "type": "actions",
        "elements": [{
            "type": "button",
            "text": plain("Submit"),
            "action_id": "form",
            "value": value,
            "style": "primary",
        }],
    }));
    blocks.push(json!({ "type": "markdown", "text": "_Answer every question, then Submit. Or reply in this thread to tell the task._" }));
    Told {
        text: text.to_owned(),
        blocks: Value::Array(blocks),
    }
}

/// What came of the need `key` on `task`, now that it is over: the line that
/// takes the place of its buttons.
pub fn over(task: &Task, key: &str) -> String {
    let decision = key
        .split_once(':')
        .and_then(|(_, id)| task.decisions.iter().find(|d| d.id.as_str() == id));
    if let Some(d) = decision {
        let by = match d.decided_by {
            Some(Decider::User) => " by you",
            Some(Decider::Controller) => " by Otter",
            Some(Decider::Policy) => " by policy",
            None => "",
        };
        return match d.status {
            DecisionStatus::Approved => format!("✅ Approved{by}."),
            DecisionStatus::Denied => format!("⛔ Denied{by}."),
            DecisionStatus::Answered => match &d.answer {
                Some(a) => format!(
                    "✅ Answered{by}: {}",
                    cut(&otter_core::redact::redact(a), 500)
                ),
                None => format!("✅ Answered{by}."),
            },
            DecisionStatus::Cancelled => "No longer asked: its run ended.".into(),
            DecisionStatus::Pending => "Asked again below.".into(),
        };
    }
    match task.status {
        TaskStatus::Done => "No longer waiting: the task is done.".into(),
        TaskStatus::Cancelled => "No longer waiting: the task was cancelled.".into(),
        s => format!("No longer waiting: the task is {}.", s.as_str()),
    }
}

/// The question a reply in the task's thread answers: its oldest waiting
/// decision, when that is a question with no options to choose from.
pub fn open_question(task: &Task) -> Option<DecisionId> {
    let n = task.needs()?;
    let d = task
        .decisions
        .iter()
        .find(|d| Some(&d.id) == n.decision_id.as_ref())?;
    (d.kind == DecisionKind::Question && d.options.is_empty()).then(|| d.id.clone())
}

/// `text` with each task named by its title rather than its id
/// (`[ft_…]` → “title”).
pub fn titled(text: &str, titles: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("[ft_") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        match (titles.get(&after[..end]), after[end..].starts_with(']')) {
            (Some(title), true) => {
                out.push_str(&format!("“{title}”"));
                rest = &after[end + 1..];
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// What `/otter` does (D-202).
pub const HELP: &str = "**/otter** — what needs you, and what is at work.
**/otter new** *what you want* — a task, made and started, with a thread of its own in your direct messages with Otter.

Write to Otter in its direct messages to talk to it. When a task needs you, Otter says so there; reply under that message to talk to the task.";

/// What needs the developer, longest waiting first, and what is at work,
/// newest first: `/otter status` (D-202).
pub fn status<'a>(tasks: impl Iterator<Item = &'a Task>) -> String {
    const SHOWN: usize = 15;
    let mut needs = Vec::new();
    let mut working = Vec::new();
    for t in tasks {
        match t.needs() {
            Some(n) => needs.push((
                n.since,
                format!("- **{}** — {}", t.title, cut(&n.summary, 200)),
            )),
            None if t.activity() == Activity::Working => working.push((
                t.updated_at,
                format!("- **{}** — {}", t.title, t.status.as_str()),
            )),
            None => {}
        }
    }
    if needs.is_empty() && working.is_empty() {
        return "Nothing needs you, and nothing is at work.".into();
    }
    needs.sort_by_key(|(since, _)| *since);
    working.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let section = |name: &str, lines: Vec<(Timestamp, String)>| {
        if lines.is_empty() {
            return None;
        }
        let n = lines.len();
        let mut out = format!("**{name}** ({n})");
        for (_, line) in lines.into_iter().take(SHOWN) {
            out.push('\n');
            out.push_str(&line);
        }
        if n > SHOWN {
            out.push_str(&format!("\n_and {} more_", n - SHOWN));
        }
        Some(out)
    };
    [section("Needs you", needs), section("At work", working)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A task's title from what the developer asked: its first line, short —
/// 80 characters, and within the bytes a title may have (200).
pub fn title_of(request: &str) -> String {
    let first = request
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Task")
        .trim();
    let mut end = 0;
    for (n, (i, c)) in first.char_indices().enumerate() {
        if n == 80 || i + c.len_utf8() > 190 {
            return format!("{}…", &first[..end]);
        }
        end = i + c.len_utf8();
    }
    first.to_owned()
}

/// At most `max` characters of `s`, with `…` when cut.
fn cut(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::task::{DecisionRequest, Risk};

    fn task(decisions: Vec<DecisionRequest>, status: TaskStatus) -> Task {
        let mut t = Task::new(
            "Export CSV".into(),
            "Add CSV export".into(),
            chrono::Utc::now(),
        );
        t.id = "ft_abc".into();
        t.decisions = decisions;
        t.status = status;
        t
    }

    fn decision(id: &str, kind: DecisionKind, options: &[&str]) -> DecisionRequest {
        DecisionRequest {
            id: id.into(),
            step_id: None,
            run_id: None,
            kind,
            risk: Risk::Medium,
            summary: "Run `rm -rf build`".into(),
            detail: None,
            command: Some("rm -rf build".into()),
            options: options.iter().map(|o| o.to_string()).collect(),
            status: DecisionStatus::Pending,
            decided_by: None,
            answer: None,
            rationale: None,
            created_at: chrono::Utc::now(),
            decided_at: None,
            user_only: false,
            grant: None,
        }
    }

    fn actions(t: &Told) -> Vec<(String, Value)> {
        t.blocks
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "actions")
            .flat_map(|b| b["elements"].as_array().unwrap().clone())
            .map(|e| {
                let v: Value = serde_json::from_str(e["value"].as_str().unwrap()).unwrap();
                (e["action_id"].as_str().unwrap().to_owned(), v)
            })
            .collect()
    }

    #[test]
    fn a_decision_is_approved_or_denied_and_says_its_command() {
        let t = task(
            vec![
                decision("dec_1", DecisionKind::ToolPermission, &[]),
                decision("dec_2", DecisionKind::ToolPermission, &[]),
            ],
            TaskStatus::Implementing,
        );
        let n = t.needs().unwrap();
        assert_eq!(key(&n), "decision:dec_1");
        let told = told(&t, &n, None);
        assert!(told.text.starts_with("**Export CSV** asks you to decide:"));
        assert!(told.text.contains("```\nrm -rf build\n```"));
        assert!(told.text.contains("1 more decision after this one"));
        let a = actions(&told);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].0, "approve");
        assert_eq!(a[0].1, json!({"task": "ft_abc", "decision": "dec_1"}));
        assert_eq!(a[1].0, "deny");
    }

    #[test]
    fn a_question_offers_its_options_or_takes_a_reply() {
        let with = task(
            vec![decision(
                "dec_q",
                DecisionKind::Question,
                &["Postgres", "SQLite"],
            )],
            TaskStatus::Implementing,
        );
        let a = actions(&told(&with, &with.needs().unwrap(), None));
        assert_eq!(
            a.iter()
                .map(|(id, v)| (id.as_str(), v["answer"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            vec![("answer:0", "Postgres"), ("answer:1", "SQLite")]
        );
        assert_eq!(open_question(&with), None);
        let without = task(
            vec![decision("dec_q", DecisionKind::Question, &[])],
            TaskStatus::Implementing,
        );
        let told = told(&without, &without.needs().unwrap(), None);
        assert!(actions(&told).is_empty());
        assert!(
            told.blocks[0]["text"]
                .as_str()
                .unwrap()
                .contains("Reply in this thread to answer.")
        );
        assert_eq!(open_question(&without), Some("dec_q".into()));
    }

    /// A form (D-202): each question in its own block, choices in a select
    /// (several where it takes several), a box where there are none, and
    /// Submit carrying the task and the decision.
    #[test]
    fn a_form_is_asked_question_by_question() {
        use otter_core::conversation::{Question, QuestionOption};
        let t = task(
            vec![decision("dec_f", DecisionKind::Question, &[])],
            TaskStatus::Implementing,
        );
        let q = |id: &str, options: &[&str], multi: bool| Question {
            id: id.into(),
            header: None,
            prompt: format!("{id}?"),
            options: options
                .iter()
                .map(|o| QuestionOption {
                    label: o.to_string(),
                    description: None,
                })
                .collect(),
            multi_select: multi,
        };
        let form = [
            q("db", &["Postgres", "SQLite"], false),
            q("langs", &["Rust", "Go"], true),
            q("name", &[], false),
        ];
        let told = told(&t, &t.needs().unwrap(), Some(&form));
        let blocks = told.blocks.as_array().unwrap();
        let by_id = |id: &str| blocks.iter().find(|b| b["block_id"] == id).unwrap();
        assert_eq!(by_id("q:db")["elements"][0]["type"], "static_select");
        assert_eq!(
            by_id("q:db")["elements"][0]["options"][1]["value"],
            "SQLite"
        );
        assert_eq!(
            by_id("q:langs")["elements"][0]["type"],
            "multi_static_select"
        );
        assert_eq!(by_id("q:name")["type"], "input");
        assert_eq!(by_id("q:name")["element"]["type"], "plain_text_input");
        let submit = blocks
            .iter()
            .flat_map(|b| b["elements"].as_array().cloned().unwrap_or_default())
            .find(|e| e["action_id"] == "form")
            .unwrap();
        let v: Value = serde_json::from_str(submit["value"].as_str().unwrap()).unwrap();
        assert_eq!(v, json!({"task": "ft_abc", "decision": "dec_f"}));
    }

    #[test]
    fn a_stopped_task_offers_retry() {
        let mut t = task(vec![], TaskStatus::Failed);
        t.status_reason = Some("Tests failed three times".into());
        let n = t.needs().unwrap();
        assert!(key(&n).starts_with("failed:"));
        let told = told(&t, &n, None);
        assert_eq!(
            told.text,
            "**Export CSV** stopped: Tests failed three times"
        );
        assert_eq!(actions(&told)[0].0, "retry");
        assert_eq!(open_question(&t), None);
    }

    #[test]
    fn what_came_of_a_need_says_who_decided() {
        let mut d = decision("dec_1", DecisionKind::ToolPermission, &[]);
        d.status = DecisionStatus::Approved;
        d.decided_by = Some(Decider::User);
        let mut q = decision("dec_q", DecisionKind::Question, &[]);
        q.status = DecisionStatus::Answered;
        q.decided_by = Some(Decider::User);
        q.answer = Some("Postgres".into());
        let t = task(vec![d, q], TaskStatus::Implementing);
        assert_eq!(over(&t, "decision:dec_1"), "✅ Approved by you.");
        assert_eq!(over(&t, "question:dec_q"), "✅ Answered by you: Postgres");
        assert_eq!(
            over(&t, "failed:123"),
            "No longer waiting: the task is implementing."
        );
    }

    /// `/otter status`: what needs the developer, longest waiting first,
    /// then what is at work; drafts and what is done aren't listed.
    #[test]
    fn status_says_what_needs_you_and_what_is_at_work() {
        assert_eq!(
            status(std::iter::empty()),
            "Nothing needs you, and nothing is at work."
        );
        let mut asks = task(
            vec![decision("dec_1", DecisionKind::ToolPermission, &[])],
            TaskStatus::Implementing,
        );
        asks.title = "Asks".into();
        let mut stopped = task(vec![], TaskStatus::Failed);
        stopped.title = "Stopped".into();
        stopped.status_reason = Some("Tests failed".into());
        stopped.status_since = Some(chrono::Utc::now() - chrono::TimeDelta::hours(1));
        let mut works = task(vec![], TaskStatus::Implementing);
        works.title = "Works".into();
        let mut draft = task(vec![], TaskStatus::Draft);
        draft.title = "Draft".into();
        let all = [asks, stopped, works, draft];
        assert_eq!(
            status(all.iter()),
            "**Needs you** (2)\n- **Stopped** — Tests failed\n- **Asks** — Run `rm -rf build`\n\n**At work** (1)\n- **Works** — implementing"
        );
    }

    #[test]
    fn a_tasks_title_is_the_first_line_it_was_asked_in() {
        assert_eq!(
            title_of("\n  Add CSV export  \nwith tests"),
            "Add CSV export"
        );
        assert_eq!(title_of(&"x".repeat(100)).chars().count(), 81);
        // Within a title's bytes, cut between characters.
        let long = title_of(&"导出".repeat(50));
        assert!(
            long.len() <= 200 && long.ends_with('…'),
            "{} {long}",
            long.len()
        );
    }

    #[test]
    fn tasks_are_named_by_their_titles() {
        let titles = HashMap::from([("ft_abc".to_owned(), "Export CSV".to_owned())]);
        assert_eq!(
            titled(
                "The only task is [ft_abc], and [ft_zzz] is gone [x].",
                &titles
            ),
            "The only task is “Export CSV”, and [ft_zzz] is gone [x]."
        );
        assert_eq!(titled("[ft_abc", &titles), "[ft_abc");
        assert_eq!(titled("no ids", &titles), "no ids");
    }
}
