//! The developer's slash commands (D-190): otterd does what a command says
//! with the tool Otter would have chosen, says what came of it, and Otter's
//! turn doesn't answer it. Driven by the `scripted` brain with Otter's tools
//! on, so a turn that did run on a command would be seen replying to it.

use super::*;
use otter_core::task::{MessageCommand, MessageRole, RunState, Task, TaskEvent, TaskStatus};

async fn host_with(more: Vec<(&'static str, String)>) -> FakeCodex {
    fake_agent_host_with("ok", "scripted", move |_| {
        more.into_iter()
            .chain([("OTTER_BRAIN_V2", "1".into())])
            .collect()
    })
    .await
}

async fn at_work(host: &TestHost, request: &str) -> String {
    let id = started_task(host, request).await;
    wait_task(host, &id, "the agent at work", |f| {
        f.status == TaskStatus::Implementing
            && f.runs.last().is_some_and(|r| r.state == RunState::Running)
    })
    .await;
    id
}

/// Send `command` with `words`; wait for Otter's line about it. The task
/// then, the command's message id and Otter's line.
async fn run(
    host: &TestHost,
    id: &str,
    command_id: &str,
    command: MessageCommand,
    words: &str,
) -> (Task, String, String) {
    let f = host
        .conn()
        .await
        .task_send_command(command_id, id, command, words, vec![])
        .await
        .unwrap();
    let msg = f
        .messages
        .iter()
        .rev()
        .find(|m| m.correlation_id.as_deref() == Some(command_id))
        .unwrap()
        .id
        .to_string();
    let key = format!("command:{msg}");
    let said = |f: &Task| -> Vec<String> {
        f.messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&key)
            })
            .map(|m| m.text.clone())
            .collect()
    };
    wait_task(host, id, "Otter's line about the command", |f| {
        !said(f).is_empty()
    })
    .await;
    // Long enough for a turn on it to have answered, had one run.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let f = host.conn().await.task_get(id).await.unwrap();
    let all = said(&f);
    assert_eq!(all.len(), 1, "one line: {all:#?}");
    let answered: Vec<&str> = f
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&msg))
        .map(|m| m.text.as_str())
        .collect();
    assert!(
        answered.is_empty(),
        "Otter's turn answered a command: {answered:#?}"
    );
    let line = all[0].clone();
    (f, msg, line)
}

async fn tool_records(host: &TestHost, id: &str, name: &str) -> (usize, usize) {
    let recs = host.conn().await.task_events(id, None).await.unwrap();
    let called = recs
        .iter()
        .filter(|r| matches!(&r.event, TaskEvent::ToolCalled { tool, .. } if tool == name))
        .count();
    let done = recs
        .iter()
        .filter(|r| matches!(&r.event, TaskEvent::ToolDone { tool, .. } if tool == name))
        .count();
    (called, done)
}

/// `/tell`: the words, not the command, reach the coding agent; the message
/// is the line as written; the tool's call is in the history.
#[tokio::test]
async fn tell_passes_the_words_on_and_no_turn_answers_it() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let (f, msg, line) = run(host, &id, "tell-1", MessageCommand::Tell, "Use tabs").await;
    assert_eq!(
        line,
        "Passed on to the coding agent as you wrote it, after its current turn."
    );
    let m = f.messages.iter().find(|m| m.id.as_str() == msg).unwrap();
    assert_eq!(m.text, "/tell Use tabs");
    assert_eq!(m.command, Some(MessageCommand::Tell));
    assert_eq!(tool_records(host, &id, "forward_to_agent").await, (1, 1));
    let conversation = f.runs.last().unwrap().conversation_id.clone().unwrap();
    let c = host
        .conn()
        .await
        .conversation_get(conversation.as_str())
        .await
        .unwrap()
        .conversation;
    let given: Vec<String> = c.turns.iter().skip(1).map(|t| t.text()).collect();
    assert_eq!(given, ["From the developer:\nUse tabs"], "{given:#?}");
}

/// A message sent just before a command is still Otter's to answer: the
/// command's line doesn't count as a reply to it.
#[tokio::test]
async fn a_message_before_a_command_is_still_answered() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let f = host
        .conn()
        .await
        .task_send("plain-1", &id, "How is it going?\nINTENT: none")
        .await
        .unwrap();
    let plain = f
        .messages
        .iter()
        .rev()
        .find(|m| m.correlation_id.as_deref() == Some("plain-1"))
        .unwrap()
        .id
        .to_string();
    run(
        host,
        &id,
        "remember-0",
        MessageCommand::Remember,
        "Tabs, not commas",
    )
    .await;
    wait_task(host, &id, "the plain message answered", |f| {
        f.messages.iter().any(|m| {
            m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&plain)
        })
    })
    .await;
}

/// `/revise` on a task in review plans again; `/remember` notes a line once.
#[tokio::test]
async fn revise_plans_again_and_remember_notes_once() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    wait_task(host, &id, "in review", |f| f.status == TaskStatus::Review).await;
    let (_, _, line) = run(
        host,
        &id,
        "revise-1",
        MessageCommand::Revise,
        "Also export JSON",
    )
    .await;
    assert_eq!(line, "Planning again from where the work is.");
    assert_eq!(tool_records(host, &id, "revise_task").await, (1, 1));
    // Planned again (the fake agent is quick: it may be in review again).
    wait_task(host, &id, "planned again", |f| {
        f.rationale.iter().any(|r| r.contains("The goal changed"))
    })
    .await;

    let (_, _, line) = run(
        host,
        &id,
        "rem-1",
        MessageCommand::Remember,
        "Tabs, not commas",
    )
    .await;
    assert_eq!(line, "Noted in this task's memory.");
    let (_, _, line) = run(
        host,
        &id,
        "rem-2",
        MessageCommand::Remember,
        "Tabs, not commas",
    )
    .await;
    assert_eq!(line, "That's in this task's memory already.");
    let m = host.conn().await.memory_get(Some(&id)).await.unwrap();
    assert_eq!(m.conversation, "- Tabs, not commas\n");
}

/// A command that can't be done says why, in words for the developer, and
/// the task goes on; a reminder is a watch.
#[tokio::test]
async fn a_refused_command_says_why_and_a_reminder_is_a_watch() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    wait_task(host, &id, "in review", |f| f.status == TaskStatus::Review).await;
    let (f, _, line) = run(host, &id, "ask-1", MessageCommand::Ask, "Where is it?").await;
    assert_eq!(
        line,
        "I couldn't ask the coding agent: the coding agent isn't at work on this task. Ask while it works, or start a task for the question."
    );
    assert_eq!(f.status, TaskStatus::Review);

    let (f, _, line) = run(
        host,
        &id,
        "remind-1",
        MessageCommand::Remind { in_minutes: 30 },
        "",
    )
    .await;
    assert_eq!(line, "I'll look at this again in 30 min.");
    let w = f.watches.last().unwrap();
    assert_eq!(w.kind, "time");
    assert_eq!(w.label, "look at this task again");
}

/// What a command needs is checked before anything is kept.
#[tokio::test]
async fn a_command_without_its_words_is_refused() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    let e = host
        .conn()
        .await
        .task_send_command("ask-empty", &id, MessageCommand::Ask, " ", vec![])
        .await
        .unwrap_err();
    assert!(e.to_string().contains("/ask needs words after it"), "{e}");
    let e = host
        .conn()
        .await
        .task_send_command(
            "remind-far",
            &id,
            MessageCommand::Remind { in_minutes: 0 },
            "",
            vec![],
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("a week"), "{e}");
}

/// The controller off leaves tasks where they are, but a command is the
/// developer acting: it is done.
#[tokio::test]
async fn a_command_is_done_with_the_controller_off() {
    let host = TestHost::with_env(&[("OTTER_CONTROLLER", "off".into())]).await;
    let dir = host.home().join("draft");
    std::fs::create_dir_all(&dir).unwrap();
    let mut conn = host.conn().await;
    conn.workspace_create(WorkspaceCreate {
        name: "draft".into(),
        source: SourceSpec::Directory {
            path: dir.to_string_lossy().into_owned(),
        },
        sessions: Some(vec![]),
        ..Default::default()
    })
    .await
    .unwrap();
    let id = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "create-draft".into(),
            title: "Orders API".into(),
            request: "Add an orders API".into(),
            workspace: Some("draft".into()),
            answer_only: false,
        })
        .await
        .unwrap()
        .id
        .to_string();
    let (f, _, line) = run(
        &host,
        &id,
        "rem-off",
        MessageCommand::Remember,
        "Use Postgres",
    )
    .await;
    assert_eq!(line, "Noted in this task's memory.");
    assert_eq!(f.status, TaskStatus::Draft);
}

/// otterd dies once the tool's change is made, before Otter's line: after
/// the restart the line is written once, and the change was made once.
#[tokio::test]
async fn a_crash_after_the_change_says_it_once_after() {
    let mut fake = host_with(vec![("OTTER_TEST_CRASH", "applied:revise_task".into())]).await;
    let id = started_task(&fake.host, "Add CSV export").await;
    wait_task(&fake.host, &id, "in review", |f| {
        f.status == TaskStatus::Review
    })
    .await;
    fake.host
        .conn()
        .await
        .task_send_command(
            "revise-crash",
            &id,
            MessageCommand::Revise,
            "Also JSON",
            vec![],
        )
        .await
        .unwrap();
    eventually("otterd dies", || {
        let gone = fake
            .host
            .daemon
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_some();
        async move { gone.then_some(()) }
    })
    .await;
    fake.host.revive_without("OTTER_TEST_CRASH").await;
    let host = &fake.host;
    let f = wait_task(host, &id, "Otter's line after the restart", |f| {
        f.messages.iter().any(|m| {
            m.role == MessageRole::Controller
                && m.correlation_id
                    .as_deref()
                    .is_some_and(|c| c.starts_with("command:"))
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let f = host.conn().await.task_get(f.id.as_str()).await.unwrap();
    let lines: Vec<&str> = f
        .messages
        .iter()
        .filter(|m| {
            m.correlation_id
                .as_deref()
                .is_some_and(|c| c.starts_with("command:"))
        })
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(lines, ["Planning again from where the work is."]);
    // Called twice (once before the crash), applied once.
    assert_eq!(tool_records(host, &id, "revise_task").await, (2, 1));
}
