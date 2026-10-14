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
    wait_feature(host, &id, "in review", |f| {
        f.status == FeatureStatus::Review
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
