//! Otter's turn with its tools (D-089, M1): what the gateway allows, in
//! which turn, once; what it records; what it never lets out. Driven by the
//! `scripted` brain with `OTTER_BRAIN_V2=1`: a message's `CALL:` lines are
//! the model's tool calls, `AS: event` runs the turn as an event's would.

use super::*;
use otter_core::feature::{
    DecisionStatus, Feature, FeatureEvent, FeatureStatus, MessageRole, RunState,
};
use serde_json::Value;

async fn host_with(more: Vec<(&'static str, String)>) -> FakeCodex {
    fake_agent_host_with("ok", "scripted", move |_| {
        more.into_iter()
            .chain([("OTTER_BRAIN_V2", "1".into())])
            .collect()
    })
    .await
}

/// Send `script` as the developer; wait for the one reply to it. Returns
/// the feature then and each call's answer, in order.
async fn run(host: &TestHost, id: &str, command_id: &str, script: &str) -> (Feature, Vec<Value>) {
    let f = host
        .conn()
        .await
        .feature_send(command_id, id, script)
        .await
        .unwrap();
    let msg = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User && m.text == script)
        .unwrap()
        .id
        .to_string();
    let replies = |f: &Feature| -> Vec<String> {
        f.messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&msg)
            })
            .map(|m| m.text.clone())
            .collect()
    };
    wait_feature(host, id, "Otter's answer", |f| !replies(f).is_empty()).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let f = host.conn().await.feature_get(id).await.unwrap();
    let all = replies(&f);
    assert_eq!(all.len(), 1, "one reply: {all:#?}");
    let answers = all[0]
        .lines()
        .filter_map(|l| {
            l.split_once(": ")
                .and_then(|(_, j)| serde_json::from_str(j).ok())
        })
        .collect();
    (f, answers)
}

fn code(answer: &Value) -> &str {
    answer["error"]["code"].as_str().unwrap_or("ok")
}

/// The feature's history records, oldest first.
async fn records(host: &TestHost, id: &str) -> Vec<otter_core::feature::FeatureEventRecord> {
    host.conn().await.feature_events(id, None).await.unwrap()
}

async fn in_review(host: &TestHost) -> String {
    let id = started_feature(host, "Add CSV export").await;
    // Its gates checked too: accepting before they are is refused.
    wait_feature(host, &id, "in review, gates checked", |f| {
        f.status == FeatureStatus::Review && !f.gates.is_empty()
    })
    .await;
    id
}

async fn at_work(host: &TestHost, request: &str) -> String {
    let id = started_feature(host, request).await;
    wait_feature(host, &id, "the agent at work", |f| {
        f.status == FeatureStatus::Implementing
            && f.runs.last().is_some_and(|r| r.state == RunState::Running)
    })
    .await;
    id
}

#[tokio::test]
async fn a_turn_an_event_started_cant_act_on_the_developers_word() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = in_review(host).await;
    let script = "AS: event\nCALL: accept_task {}\nCALL: resume_task {}\nCALL: retry_task {}\nCALL: start_task {}\nCALL: revise_task {}";
    let (f, answers) = run(host, &id, "ev-1", script).await;
    assert_eq!(answers.len(), 5, "{answers:#?}");
    for a in &answers {
        assert_eq!(code(a), "not_allowed_in_this_turn", "{a}");
        assert!(a["error"]["next"].is_string(), "{a}");
    }
    assert_eq!(f.status, FeatureStatus::Review);
    // The developer's own word does it.
    let (f, answers) = run(host, &id, "dev-1", "Ship it\nCALL: accept_task {}").await;
    assert_eq!(code(&answers[0]), "ok", "{answers:#?}");
    assert_eq!(answers[0]["data"]["done"], "accepted");
    assert_eq!(f.status, FeatureStatus::Done);
}

#[tokio::test]
async fn a_turn_an_event_started_may_pause() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let (_, answers) = run(host, &id, "ev-pause", "AS: event\nCALL: pause_task {}").await;
    assert_eq!(code(&answers[0]), "ok", "{answers:#?}");
    let f = wait_feature(host, &id, "paused, its run over", |f| {
        f.status == FeatureStatus::Paused && f.runs.iter().all(|r| r.state.is_over())
    })
    .await;
    assert_eq!(f.status_reason.as_deref(), Some("Paused by Otter"));
    // Recorded as Otter's, with no developer message behind it.
    let called = records(host, &id)
        .await
        .into_iter()
        .find_map(|r| match r.event {
            FeatureEvent::ToolCalled {
                tool,
                origin,
                authority,
            } if tool == "pause_task" => Some((origin, authority)),
            _ => None,
        })
        .unwrap();
    assert_eq!(called, ("event".to_owned(), None));
}

#[tokio::test]
async fn only_this_turns_messages_go_to_the_agent_as_written() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let (f, _) = run(host, &id, "q-1", "Use tabs\nINTENT: none").await;
    let earlier = f
        .messages
        .iter()
        .find(|m| m.text.starts_with("Use tabs"))
        .unwrap();
    // An earlier message (answered already) isn't this turn's to pass on.
    let script = format!(
        "CALL: forward_to_agent {{\"message_ids\":[\"{}\"]}}",
        earlier.id
    );
    let (f, answers) = run(host, &id, "q-2", &script).await;
    assert_eq!(code(&answers[0]), "invalid", "{answers:#?}");
    // This turn's own message goes, as the developer wrote it.
    let text = "Use semicolons\nINTENT: guide";
    let (f2, answers) = run(host, &id, "q-3", text).await;
    assert_eq!(answers[0]["data"]["done"], "passed on", "{answers:#?}");
    let c = host
        .conn()
        .await
        .conversation_get(
            f2.runs
                .last()
                .unwrap()
                .conversation_id
                .as_ref()
                .unwrap()
                .as_str(),
        )
        .await
        .unwrap()
        .conversation;
    let given: Vec<String> = c.turns.iter().skip(1).map(|t| t.text()).collect();
    assert_eq!(
        given,
        [format!("From the developer:\n{text}")],
        "{given:#?}"
    );
    // No message was added in the developer's name.
    let mine = |f: &Feature| {
        f.messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count()
    };
    assert_eq!(mine(&f2), mine(&f) + 1);
}

#[tokio::test]
async fn a_turn_that_fails_still_answers_once() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let (f, _) = run(host, &id, "fail-1", "What's up?\nFAIL").await;
    let reply = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Controller)
        .unwrap();
    assert!(!reply.text.starts_with("Scripted"), "{}", reply.text);
    assert!(
        reply.text.contains("task 1 of 1"),
        "the status: {}",
        reply.text
    );
    // Said, not only logged (D-076); and `otter doctor` has it until a turn works.
    assert!(
        reply
            .text
            .contains("I couldn't look just now: scripted failure."),
        "{}",
        reply.text
    );
    let couldnt = |s: &otter_core::HostStatus| {
        s.problems
            .iter()
            .any(|p| p.what.starts_with("Otter couldn't answer on"))
    };
    assert!(couldnt(&host.conn().await.host_status().await.unwrap()));
    run(host, &id, "fail-2", "And now?\nCALL: get_task {}").await;
    assert!(!couldnt(&host.conn().await.host_status().await.unwrap()));
}

#[tokio::test]
async fn a_turn_out_of_time_answers_once_and_says_what_it_did() {
    let fake = host_with(vec![("OTTER_TURN_DEADLINE_MS", "1500".into())]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let (f, _) = run(
        host,
        &id,
        "slow-1",
        "Hold on\nCALL: pause_task {}\nSLEEP: 4000",
    )
    .await;
    let reply = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Controller)
        .unwrap();
    assert!(
        reply.text.contains("Done: pause_task: paused."),
        "{}",
        reply.text
    );
    assert_eq!(f.status, FeatureStatus::Paused);
}

#[tokio::test]
async fn a_change_is_made_once_and_written_down_before() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let (_, answers) = run(
        host,
        &id,
        "rev-1",
        "Also JSON\nCALL: revise_task {}\nCALL: revise_task {}\nCALL: get_task {}",
    )
    .await;
    assert_eq!(answers[0]["data"]["done"], "planning again", "{answers:#?}");
    assert_eq!(answers[1]["data"]["done"], "already", "{answers:#?}");
    let recs = records(host, &id).await;
    let replans = recs
        .iter()
        .filter(|r| r.text.contains("You changed the goal: planning again"))
        .count();
    assert_eq!(replans, 1);
    // The change: its intent, then the change with its outcome.
    let seq = |want: &dyn Fn(&FeatureEvent) -> bool| {
        recs.iter().find(|r| want(&r.event)).map(|r| r.seq).unwrap()
    };
    let called =
        seq(&|e| matches!(e, FeatureEvent::ToolCalled { tool, .. } if tool == "revise_task"));
    let done = seq(
        &|e| matches!(e, FeatureEvent::ToolDone { tool, error: None } if tool == "revise_task"),
    );
    assert!(called < done, "{called} < {done}");
    // A look changes nothing and is written once, after.
    assert!(
        !recs.iter().any(
            |r| matches!(&r.event, FeatureEvent::ToolCalled { tool, .. } if tool == "get_task")
        )
    );
    assert!(recs.iter().any(
        |r| matches!(&r.event, FeatureEvent::ToolDone { tool, error: None } if tool == "get_task")
    ));
}

#[tokio::test]
async fn a_turn_has_so_many_calls() {
    let fake = host_with(vec![("OTTER_TURN_MAX_CALLS", "2".into())]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let (_, answers) = run(
        host,
        &id,
        "many-1",
        "CALL: get_task {}\nCALL: get_task {}\nCALL: get_task {}",
    )
    .await;
    let codes: Vec<&str> = answers.iter().map(code).collect();
    assert_eq!(codes, ["ok", "ok", "limit"]);
}

#[tokio::test]
async fn otter_sees_what_waits_on_the_developer_and_cant_settle_it() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_feature(host, "Clean up ASK_RM").await;
    wait_feature(host, &id, "asking about rm", |f| {
        f.pending_decisions().any(|d| d.user_only)
    })
    .await;
    let (f, answers) = run(
        host,
        &id,
        "look-1",
        "Why is it stuck?\nCALL: get_task {}\nCALL: get_pending_decisions {}\nCALL: get_run_events {}\nCALL: resume_task {}",
    )
    .await;
    assert_eq!(answers[0]["data"]["status"], "blocked", "{answers:#?}");
    let d = &answers[1]["data"]["decisions"][0];
    assert_eq!(d["developer_only"], true, "{d}");
    assert!(d["summary"].as_str().unwrap().contains("rm"), "{d}");
    assert!(
        answers[2]["data"]["run"]["state"].is_string(),
        "{}",
        answers[2]
    );
    assert_eq!(code(&answers[3]), "needs_developer", "{}", answers[3]);
    assert!(
        f.decisions
            .iter()
            .any(|d| d.user_only && d.status == DecisionStatus::Pending)
    );
    assert_eq!(f.status, FeatureStatus::Blocked);
}

#[tokio::test]
async fn otter_sees_no_secrets() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_feature(host, "Deploy it LEAK").await;
    wait_feature(host, &id, "its run over", |f| {
        f.runs.first().is_some_and(|r| r.state.is_over())
    })
    .await;
    let (f, answers) = run(
        host,
        &id,
        "leak-1",
        "What went wrong?\nCALL: get_run_events {}\nCALL: get_task_history {}\nCALL: get_task {}\nCALL: get_task_evidence {}",
    )
    .await;
    assert_eq!(answers.len(), 4);
    let reply = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Controller)
        .unwrap();
    for secret in ["LEAKLEAK", "leakyleakybearer"] {
        assert!(!reply.text.contains(secret), "{secret} in {}", reply.text);
    }
}

/// A host whose Otter is Claude, through a fake worker
/// (`tests/fake_claude_worker.sh`, `FAKE_WORKER_MODE`).
async fn claude_host(worker_mode: &str) -> FakeCodex {
    let mode = worker_mode.to_owned();
    fake_agent_host_with("ok", "claude", move |scratch| {
        let worker = scratch.join("worker.sh");
        std::fs::write(&worker, include_str!("../fake_claude_worker.sh")).unwrap();
        vec![
            ("OTTER_BRAIN_V2", "1".into()),
            ("OTTER_NODE", "/bin/sh".into()),
            ("OTTER_CLAUDE_WORKER", worker.to_string_lossy().into_owned()),
            ("FAKE_WORKER_MODE", mode),
        ]
    })
    .await
}

/// A draft feature in a workspace of its own: nothing plans it.
async fn draft(host: &TestHost) -> String {
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
    conn.feature_create(otter_protocol::feature::FeatureCreate {
        command_id: "create-draft".into(),
        title: "CSV export".into(),
        request: "Add CSV export".into(),
        workspace: Some("draft".into()),
        answer_only: false,
    })
    .await
    .unwrap()
    .id
    .to_string()
}

#[tokio::test]
async fn claude_answers_through_the_worker_with_otters_tools() {
    let fake = claude_host("ok").await;
    let host = &fake.host;
    let status = host.conn().await.host_status().await.unwrap();
    assert_eq!(status.otter.as_deref(), Some("with tools (claude)"));
    assert!(
        !status.problems.iter().any(|p| p.what.starts_with("Otter")),
        "{:?}",
        status.problems
    );
    let id = draft(host).await;
    let text = "Where is it? FORWARD";
    let f = host
        .conn()
        .await
        .feature_send("w-1", &id, text)
        .await
        .unwrap();
    let msg = f
        .messages
        .iter()
        .rev()
        .find(|m| m.text == text)
        .unwrap()
        .id
        .clone();
    let f = wait_feature(host, &id, "Otter's answer", |f| {
        f.messages
            .iter()
            .any(|m| m.correlation_id.as_deref() == Some(msg.as_str()))
    })
    .await;
    let reply = f
        .messages
        .iter()
        .find(|m| m.correlation_id.as_deref() == Some(msg.as_str()))
        .unwrap();
    // It looked the task up, then passed the developer's message on by its id.
    assert_eq!(reply.text, "The task is draft; your message: passed on.");
    let recs = records(host, &id).await;
    let forwarded = recs.iter().find_map(|r| match &r.event {
        FeatureEvent::ToolCalled {
            tool, authority, ..
        } if tool == "forward_to_agent" => Some(authority.clone()),
        _ => None,
    });
    assert_eq!(forwarded, Some(Some(msg.to_string())));
    assert!(recs.iter().any(
        |r| matches!(&r.event, FeatureEvent::ToolDone { tool, error: None } if tool == "get_task")
    ));
}

#[tokio::test]
async fn a_worker_without_otters_profile_leaves_otter_without_tools_and_says_so() {
    let fake = claude_host("coding-only").await;
    let status = fake.host.conn().await.host_status().await.unwrap();
    let how = status.otter.unwrap();
    assert!(how.starts_with("without tools: "), "{how}");
    assert!(how.contains("no Otter profile"), "{how}");
    assert!(
        status
            .problems
            .iter()
            .any(|p| p.what.starts_with("Otter answers without tools")),
        "{:?}",
        status.problems
    );
}

/// Real Claude through the built worker, the whole way: a task blocked
/// before any planning (no workspace), asked why. Run by hand:
/// `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime cargo test -p otterd
/// --test e2e live_otter_says_why_a_task_is_blocked -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
async fn live_otter_says_why_a_task_is_blocked() {
    use otter_core::feature::FeatureAction;
    let worker = std::env::var("OTTER_CLAUDE_WORKER").expect("OTTER_CLAUDE_WORKER");
    let node = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v node"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let host = TestHost::with_env(&[
        ("OTTER_CONTROLLER", "claude".into()),
        ("OTTER_CONTROLLER_MODEL", "haiku".into()),
        ("OTTER_BRAIN_V2", "1".into()),
        ("OTTER_CLAUDE_WORKER", worker),
        ("OTTER_NODE", node),
        ("OTTER_CONTROLLER_TICK_MS", "200".into()),
    ])
    .await;
    assert_eq!(
        host.conn()
            .await
            .host_status()
            .await
            .unwrap()
            .otter
            .as_deref(),
        Some("with tools (claude)")
    );
    let mut conn = host.conn().await;
    let f = conn
        .feature_create(otter_protocol::feature::FeatureCreate {
            command_id: "create-live".into(),
            title: "CSV export".into(),
            request: "Add CSV export".into(),
            workspace: None,
            answer_only: false,
        })
        .await
        .unwrap();
    let id = f.id.to_string();
    conn.feature_act("start", &id, FeatureAction::Start)
        .await
        .unwrap();
    let f = wait_feature(&host, &id, "blocked", |f| {
        f.status == FeatureStatus::Blocked
    })
    .await;
    println!("status: {:?} — {:?}", f.status, f.status_reason);
    let (f, _) = run(&host, &id, "live-why", "Why is this task blocked?").await;
    let reply = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Controller)
        .unwrap();
    println!("reply: {}", reply.text);
    let looked: Vec<String> = records(&host, &id)
        .await
        .into_iter()
        .filter_map(|r| match r.event {
            FeatureEvent::ToolDone { tool, .. } => Some(tool),
            _ => None,
        })
        .collect();
    // The context already says why; looking further is the model's call.
    println!("tools: {looked:?}");
    assert!(
        reply.text.to_lowercase().contains("workspace"),
        "{}",
        reply.text
    );
    assert!(!reply.text.contains("INTENT:"), "{}", reply.text);
    assert_eq!(f.status, FeatureStatus::Blocked, "nothing changed");
}

/// A brain without a tool-using turn (a provider's API, M5) answers as
/// before with the flag on, and the host says so.
#[tokio::test]
async fn a_provider_without_a_turn_answers_as_before_and_says_so() {
    let fake =
        fake_agent_host_with("ok", "openrouter", |_| vec![("OTTER_BRAIN_V2", "1".into())]).await;
    let host = &fake.host;
    let status = host.conn().await.host_status().await.unwrap();
    assert_eq!(
        status.otter.as_deref(),
        Some("without tools: `openrouter` has no tool-using turn yet")
    );
    assert!(
        status
            .problems
            .iter()
            .any(|p| p.what.starts_with("Otter answers without tools")),
        "{:?}",
        status.problems
    );
    let id = draft(host).await;
    let (_, answers) = run(host, &id, "or-1", "What's going on?\nCALL: get_task {}").await;
    assert!(answers.is_empty(), "no tool answered: {answers:?}");
    assert!(
        !records(host, &id)
            .await
            .iter()
            .any(|r| r.text.starts_with("Otter: ")),
        "converse uses no tools"
    );
}

/// Otter's tools are a host setting (D-089 M2a), off by default: turned on
/// from Settings, they answer the next message, and stay on across a restart.
#[tokio::test]
async fn otters_tools_are_a_host_setting() {
    use otter_protocol::host::SettingsUpdate;
    let mut fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let s = host.conn().await.settings_get().await.unwrap();
    assert_eq!(s.otter_tools, Some(false));
    assert_eq!(s.otter_tools_from_env, None);
    assert_eq!(
        s.otter_answers.as_deref(),
        Some("without tools (off in Settings)")
    );
    let id = at_work(host, "Add CSV export HANG").await;
    let (_, answers) = run(host, &id, "set-1", "Look\nCALL: get_task {}").await;
    assert!(answers.is_empty(), "off: no tools: {answers:?}");
    let s = host
        .conn()
        .await
        .settings_set(SettingsUpdate {
            otter_tools: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.otter_tools, Some(true));
    assert_eq!(s.otter_answers.as_deref(), Some("with tools (scripted)"));
    let (_, answers) = run(host, &id, "set-2", "Look again\nCALL: get_task {}").await;
    assert_eq!(code(&answers[0]), "ok", "{answers:?}");
    fake.host.restart().await;
    let s = fake.host.conn().await.settings_get().await.unwrap();
    assert_eq!(s.otter_tools, Some(true));
}

/// Otter's reply streams as Claude writes it (D-051, plan M2b): what it says
/// before looking, then the answer; the last piece is the reply itself.
#[tokio::test]
async fn otters_reply_streams_as_it_is_written() {
    use otter_protocol::Event;
    let fake = claude_host("ok").await;
    let host = &fake.host;
    let id = draft(host).await;
    let mut events = host.conn().await.subscribe(None).await.unwrap();
    host.conn()
        .await
        .feature_send("stream-1", &id, "Where is it?")
        .await
        .unwrap();
    let mut pieces = Vec::new();
    let last = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let rec = events.next().await.unwrap().unwrap();
            if let Event::FeatureStream {
                role: MessageRole::Controller,
                text,
                done,
                ..
            } = rec.event
            {
                if done {
                    return text;
                }
                pieces.push(text);
            }
        }
    })
    .await
    .expect("the stream finishes");
    // Sent at most every 100 ms: the first piece goes at once, the rest with
    // the reply (the fake writes them all inside that).
    assert_eq!(
        pieces.first().map(String::as_str),
        Some("Let me "),
        "{pieces:?}"
    );
    assert_eq!(last, "The task is draft.");
}

/// The history records Otter's tool `name` left, as (called, done).
async fn tool_records(host: &TestHost, id: &str, name: &str) -> (usize, usize) {
    let recs = records(host, id).await;
    let called = recs
        .iter()
        .filter(|r| matches!(&r.event, FeatureEvent::ToolCalled { tool, .. } if tool == name))
        .count();
    let done = recs
        .iter()
        .filter(|r| matches!(&r.event, FeatureEvent::ToolDone { tool, .. } if tool == name))
        .count();
    (called, done)
}

/// otterd dies once a change is written down and before it is made (plan
/// §8): the next start answers the message again, and the change is made
/// once.
#[tokio::test]
async fn a_crash_before_a_change_makes_it_once_after() {
    let mut fake = host_with(vec![("OTTER_TEST_CRASH", "called:revise_task".into())]).await;
    let id = at_work(&fake.host, "Add CSV export HANG").await;
    let text = "Also export JSON\nCALL: revise_task {}";
    fake.host
        .conn()
        .await
        .feature_send("crash-1", &id, text)
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
    let f = wait_feature(host, &id, "answered after the restart", |f| {
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::Controller && m.text.contains("revise_task"))
    })
    .await;
    let msg = f.messages.iter().find(|m| m.text == text).unwrap();
    let replies = f
        .messages
        .iter()
        .filter(|m| m.correlation_id.as_deref() == Some(msg.id.as_str()))
        .count();
    assert_eq!(replies, 1);
    // Written down twice (before the crash, and again), made once.
    assert_eq!(tool_records(host, &id, "revise_task").await, (2, 1));
    let replans = records(host, &id)
        .await
        .iter()
        .filter(|r| r.text.contains("You changed the goal: planning again"))
        .count();
    assert_eq!(replans, 1);
}

/// otterd dies once a forward is recorded as done, before the message
/// reaches the agent (plan §8): after the restart it reaches the agent's
/// conversation once — not lost, not twice.
#[tokio::test]
async fn a_crash_after_a_forward_still_delivers_it_once() {
    let mut fake = host_with(vec![(
        "OTTER_TEST_CRASH",
        "applied:forward_to_agent".into(),
    )])
    .await;
    let id = at_work(&fake.host, "Add CSV export HANG").await;
    let text = "Use tabs, not commas\nINTENT: guide";
    fake.host
        .conn()
        .await
        .feature_send("crash-2", &id, text)
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
    let f = wait_feature(host, &id, "answered after the restart", |f| {
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::Controller && m.text.contains("forward_to_agent"))
    })
    .await;
    assert_eq!(tool_records(host, &id, "forward_to_agent").await, (2, 1));
    let conversation = f.runs.last().unwrap().conversation_id.clone().unwrap();
    let carrying = |c: &otter_core::conversation::Conversation| {
        c.turns
            .iter()
            .filter(|t| t.text().contains("Use tabs, not commas"))
            .count()
    };
    eventually("the message reaches the agent", || async {
        let c = host
            .conn()
            .await
            .conversation_get(conversation.as_str())
            .await
            .unwrap()
            .conversation;
        (carrying(&c) > 0).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let c = host
        .conn()
        .await
        .conversation_get(conversation.as_str())
        .await
        .unwrap()
        .conversation;
    assert_eq!(
        carrying(&c),
        1,
        "{:#?}",
        c.turns.iter().map(|t| t.text()).collect::<Vec<_>>()
    );
}

/// The same message sent twice (a retry after a lost reply) is one message,
/// answered once.
#[tokio::test]
async fn a_message_sent_twice_is_answered_once() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let text = "Where is it?\nCALL: get_task {}";
    for _ in 0..2 {
        host.conn()
            .await
            .feature_send("twice-1", &id, text)
            .await
            .unwrap();
    }
    let (f, answers) = run(host, &id, "twice-1", text).await;
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(f.messages.iter().filter(|m| m.text == text).count(), 1);
    assert_eq!(tool_records(host, &id, "get_task").await, (0, 1));
}

/// A host whose coding agent runs on the SDK runtime (a fake worker,
/// `FAKE_WORKER_MODE`), with Otter's tools on and the `scripted` brain.
async fn sdk_host(worker_mode: &str) -> FakeCodex {
    sdk_host_with(worker_mode, vec![]).await
}

async fn sdk_host_with(worker_mode: &str, more: Vec<(&'static str, String)>) -> FakeCodex {
    let mode = worker_mode.to_owned();
    fake_agent_host_with("ok", "scripted", move |scratch| {
        let worker = scratch.join("worker.sh");
        std::fs::write(&worker, include_str!("../fake_claude_worker.sh")).unwrap();
        vec![
            ("OTTER_BRAIN_V2", "1".into()),
            ("OTTER_CODING_BACKEND", "sdk".into()),
            ("OTTER_NODE", "/bin/sh".into()),
            ("OTTER_CLAUDE_WORKER", worker.to_string_lossy().into_owned()),
            ("FAKE_WORKER_MODE", mode),
        ]
        .into_iter()
        .chain(more)
        .collect()
    })
    .await
}

/// A question while the agent works (D-089 M2.5): a turn of its own after
/// the work's, in which Otter's policy lets it only read; its answer is the
/// agent's message, and the run's result stays the work's.
#[tokio::test]
async fn a_question_at_work_is_answered_by_the_agent_reading_only() {
    let fake = sdk_host("slow").await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export").await;
    let (_, answers) = run(host, &id, "ask-1", "Where is the export?\nASK").await;
    assert_eq!(answers[0]["data"]["done"], "asked", "{answers:?}");
    assert_eq!(answers[0]["data"]["turns"], 1, "{answers:?}");
    let f = wait_feature(host, &id, "the agent's answer", |f| {
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::Agent && m.text.contains("src/export.rs:12"))
    })
    .await;
    let answer = f
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Agent && m.text.contains("src/export.rs:12"))
        .unwrap();
    // It tried to write and to run a command: both denied, whatever policy
    // would otherwise allow.
    assert!(
        answer.text.contains("(Write: deny; Bash: deny.)"),
        "{}",
        answer.text
    );
    let c = host
        .conn()
        .await
        .conversation_get(f.runs[0].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap()
        .conversation;
    let asked: Vec<_> = c.turns.iter().filter(|t| t.read_only).collect();
    assert_eq!(asked.len(), 1, "{:#?}", c.turns);
    assert!(asked[0].text().contains("Where is the export?"));
    // The run's result is the work's, and the question cost no attempt.
    let f = wait_feature(host, &id, "the run over", |f| {
        f.runs.first().is_some_and(|r| r.state.is_over())
    })
    .await;
    assert_eq!(f.runs[0].summary.as_deref(), Some("Did the work."));
    assert_eq!(f.runs[0].state, RunState::Completed);
    assert_eq!(f.budget.iterations_used, 1);
}

#[tokio::test]
async fn a_question_needs_the_sdk_runtime() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export HANG").await;
    let (_, answers) = run(host, &id, "ask-legacy", "Where is it?\nASK").await;
    assert_eq!(code(&answers[0]), "not_allowed_in_this_turn", "{answers:?}");
    assert!(
        answers[0]["error"]["next"]
            .as_str()
            .unwrap()
            .contains("SDK"),
        "{answers:?}"
    );
}

#[tokio::test]
async fn a_question_with_nothing_at_work_is_a_task_of_its_own() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = in_review(host).await;
    let (_, answers) = run(host, &id, "ask-review", "Where is it?\nASK").await;
    assert_eq!(code(&answers[0]), "not_ready", "{answers:?}");
    assert!(
        answers[0]["error"]["next"]
            .as_str()
            .unwrap()
            .contains("task of its own"),
        "{answers:?}"
    );
}

/// The turn's mode is the journal's: a restart keeps it read-only.
#[tokio::test]
async fn a_question_turn_stays_read_only_after_a_restart() {
    let mut fake = sdk_host("hang").await;
    let id = at_work(&fake.host, "Add CSV export").await;
    run(&fake.host, &id, "ask-hang", "Where is it?\nASK").await;
    fake.host.restart().await;
    let f = fake.host.conn().await.feature_get(&id).await.unwrap();
    let c = fake
        .host
        .conn()
        .await
        .conversation_get(f.runs[0].conversation_id.as_ref().unwrap().as_str())
        .await
        .unwrap()
        .conversation;
    let asked: Vec<_> = c
        .turns
        .iter()
        .filter(|t| t.text().contains("a question."))
        .collect();
    assert_eq!(asked.len(), 1, "{:#?}", c.turns);
    assert!(asked[0].read_only);
}

/// Real Claude as the coding agent, through the built worker (D-089
/// M2.5): asked a question while it works — one that also tells it to
/// write a file — it answers after the work, reading only; the file isn't
/// written. Run by hand: `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime
/// cargo test -p otterd --test e2e live_a_question_is_answered_reading_only
/// -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
async fn live_a_question_is_answered_reading_only() {
    use otter_protocol::host::{CodingSettings, SettingsUpdate};
    let worker = std::env::var("OTTER_CLAUDE_WORKER").expect("OTTER_CLAUDE_WORKER");
    let node = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v node"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let host = TestHost::with_env(&[
        ("OTTER_CODING_BACKEND", "sdk".into()),
        ("OTTER_CLAUDE_WORKER", worker),
        ("OTTER_NODE", node),
        ("OTTER_CONTROLLER", "scripted".into()),
        ("OTTER_BRAIN_V2", "1".into()),
        ("OTTER_PLAN_TURN", "0".into()),
        ("OTTER_CONTROLLER_TICK_MS", "200".into()),
        ("DEVELOPER_DIR", String::new()),
        ("SDKROOT", String::new()),
    ])
    .await;
    host.conn()
        .await
        .settings_set(SettingsUpdate {
            coding: Some(CodingSettings {
                backend: None,
                model: Some("haiku".into()),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let ws = "liveask";
    let id = started_in(
        &host,
        ws,
        "Work only in the current working directory. Use the Bash tool to run exactly `sleep 20`, then create ./done.txt containing the word ok. Nothing else.",
    )
    .await;
    wait_feature(&host, &id, "the agent at work", |f| {
        f.runs.last().is_some_and(|r| r.state == RunState::Running)
    })
    .await;
    run(
        &host,
        &id,
        "live-ask",
        "Create ./question.txt containing yes, then tell me which files are in the working directory.\nASK",
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let f = loop {
        let f = host.conn().await.feature_get(&id).await.unwrap();
        let answered = f
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Agent)
            .count()
            >= 2;
        if answered && f.runs.first().is_some_and(|r| r.state.is_over()) {
            break f;
        }
        assert!(std::time::Instant::now() < deadline, "{:#?}", f.messages);
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    for m in f.messages.iter().filter(|m| m.role == MessageRole::Agent) {
        println!("agent: {}", m.text);
    }
    println!("run summary: {:?}", f.runs[0].summary);
    let dir = host.home().join(ws);
    assert!(dir.join("done.txt").exists(), "the work was done");
    assert!(
        !dir.join("question.txt").exists(),
        "the question's turn wrote nothing"
    );
}

/// An answer-only task (D-092) in a project of its own, started.
async fn answer_only_task(host: &TestHost, request: &str) -> Result<String, String> {
    let dir = host.home().join("ao");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    let mut conn = host.conn().await;
    conn.workspace_create(WorkspaceCreate {
        name: "ao".into(),
        source: SourceSpec::Directory {
            path: dir.to_string_lossy().into_owned(),
        },
        sessions: Some(vec![]),
        ..Default::default()
    })
    .await
    .unwrap();
    let f = conn
        .feature_create(otter_protocol::feature::FeatureCreate {
            command_id: "create-ao".into(),
            title: "CSV export".into(),
            request: request.into(),
            workspace: Some("ao".into()),
            answer_only: true,
        })
        .await
        .map_err(|e| e.to_string())?;
    assert!(f.answer_only);
    conn.feature_act(
        "start-ao",
        f.id.as_str(),
        otter_core::feature::FeatureAction::Start,
    )
    .await
    .unwrap();
    Ok(f.id.to_string())
}

/// An answer-only task (D-092): the agent's look at the code only reads —
/// its write is denied — and its answer ends the task.
#[tokio::test]
async fn an_answer_only_task_reads_and_answers() {
    let fake = sdk_host_with("ok", vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = answer_only_task(host, "How does the export work? NOPLAN")
        .await
        .unwrap();
    let f = wait_feature(host, &id, "answered", |f| f.status == FeatureStatus::Done).await;
    let answer = f
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Agent)
        .unwrap();
    assert_eq!(
        answer.text,
        "The export lives in export.sh:3. (Write: deny.)"
    );
    assert!(f.status_reason.unwrap().starts_with("Answered"));
}

/// A plan under answer-only goes to the developer first (D-092): approved,
/// the task may change code and the plan is set.
#[tokio::test]
async fn a_plan_under_answer_only_waits_for_the_developer() {
    use otter_core::feature::{DecisionKind, FeatureAction};
    let fake = sdk_host_with("ok", vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = answer_only_task(host, "Add CSV export").await.unwrap();
    let f = wait_feature(host, &id, "asking about the plan", |f| {
        f.pending_decisions()
            .any(|d| d.kind == DecisionKind::PlanApproval)
    })
    .await;
    let d = f.pending_decisions().next().unwrap().clone();
    assert!(d.user_only);
    assert!(
        d.detail.as_deref().unwrap().contains("only answers"),
        "{d:?}"
    );
    assert!(f.tasks.iter().all(|t| t.planning), "nothing set yet");
    let asked = f
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Controller && m.text.contains("proposes changes"))
        .unwrap();
    assert!(
        asked.text.contains("1. Write the exporter"),
        "{}",
        asked.text
    );
    host.conn()
        .await
        .feature_act(
            "allow-changes",
            &id,
            FeatureAction::Decide {
                decision_id: d.id.clone(),
                approve: true,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_feature(host, &id, "planned", |f| {
        f.tasks.iter().any(|t| t.title == "Write the exporter")
    })
    .await;
    assert!(!f.answer_only);
    assert!(
        f.rationale
            .iter()
            .any(|r| r.contains("no longer answer-only")),
        "{:?}",
        f.rationale
    );
}

/// Turned down, the plan is the answer, and nothing is built (D-092).
#[tokio::test]
async fn a_plan_turned_down_under_answer_only_is_the_answer() {
    use otter_core::feature::{DecisionKind, FeatureAction};
    let fake = sdk_host_with("ok", vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = answer_only_task(host, "Add CSV export").await.unwrap();
    let f = wait_feature(host, &id, "asking about the plan", |f| {
        f.pending_decisions()
            .any(|d| d.kind == DecisionKind::PlanApproval)
    })
    .await;
    let d = f.pending_decisions().next().unwrap().id.clone();
    host.conn()
        .await
        .feature_act(
            "keep-answer",
            &id,
            FeatureAction::Decide {
                decision_id: d,
                approve: false,
                answer: None,
                answers: None,
            },
        )
        .await
        .unwrap();
    let f = wait_feature(host, &id, "answered", |f| f.status == FeatureStatus::Done).await;
    assert!(f.answer_only);
    let answer = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Agent)
        .unwrap();
    assert!(
        answer.text.contains("1. Write the exporter"),
        "{}",
        answer.text
    );
    assert!(answer.text.contains("Not carried out"), "{}", answer.text);
    assert!(
        f.tasks.iter().all(|t| t.planning),
        "nothing planned: {:?}",
        f.tasks
    );
    assert_eq!(f.budget.iterations_used, 0);
}

/// Never an answer-only task that can't be kept to reading (D-092).
#[tokio::test]
async fn answer_only_needs_the_sdk_runtime() {
    let fake = host_with(vec![]).await;
    let e = answer_only_task(&fake.host, "How does it work?")
        .await
        .unwrap_err();
    assert!(e.contains("SDK runtime"), "{e}");
}

/// Otter turns it on on the developer's word, not on an event's (D-092).
#[tokio::test]
async fn otter_sets_answer_only_on_the_developers_word() {
    let fake = sdk_host("hang").await;
    let host = &fake.host;
    let id = at_work(host, "Add CSV export").await;
    let (f, answers) = run(
        host,
        &id,
        "ao-ev",
        "AS: event\nCALL: set_answer_only {\"on\": true}",
    )
    .await;
    assert_eq!(code(&answers[0]), "not_allowed_in_this_turn", "{answers:?}");
    assert!(!f.answer_only);
    let (f, answers) = run(
        host,
        &id,
        "ao-dev",
        "Don't change anything\nCALL: set_answer_only {\"on\": true}",
    )
    .await;
    assert_eq!(answers[0]["data"]["done"], "answer only", "{answers:?}");
    assert!(f.answer_only);
}

/// Real Claude as the coding agent on an answer-only task (D-092) whose
/// request also asks it to write a file: its look at the code only reads,
/// the file isn't written, and it answers (or its plan comes to the
/// developer). Run by hand: `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime
/// cargo test -p otterd --test e2e live_an_answer_only_task -- --ignored
/// --nocapture`.
#[tokio::test]
#[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
async fn live_an_answer_only_task_writes_nothing() {
    use otter_protocol::host::{CodingSettings, SettingsUpdate};
    let worker = std::env::var("OTTER_CLAUDE_WORKER").expect("OTTER_CLAUDE_WORKER");
    let node = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v node"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let host = TestHost::with_env(&[
        ("OTTER_CODING_BACKEND", "sdk".into()),
        ("OTTER_CLAUDE_WORKER", worker),
        ("OTTER_NODE", node),
        ("OTTER_CONTROLLER", "scripted".into()),
        ("OTTER_CONTROLLER_TICK_MS", "200".into()),
        ("DEVELOPER_DIR", String::new()),
        ("SDKROOT", String::new()),
    ])
    .await;
    host.conn()
        .await
        .settings_set(SettingsUpdate {
            coding: Some(CodingSettings {
                backend: None,
                model: Some("haiku".into()),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let id = answer_only_task(
        &host,
        "What does `make test` do in this project? Write your answer into ./answer.txt too.",
    )
    .await
    .unwrap();
    let f = wait_feature(&host, &id, "answered or asking", |f| {
        f.status == FeatureStatus::Done || f.pending_decisions().next().is_some()
    })
    .await;
    for m in &f.messages {
        println!("{:?}: {}", m.role, m.text);
    }
    println!("status: {:?} — {:?}", f.status, f.status_reason);
    assert!(
        !host.home().join("ao").join("answer.txt").exists(),
        "an answer-only task wrote nothing"
    );
}
