//! What a developer's message does today (D-089, M0): one test for each
//! route through the controller's `converse`, driven by the `scripted`
//! brain (a message's own `INTENT:` line says what it asks). Otter's reply
//! changes with Brain Runtime V2; what these tests pin must not — they pass
//! before it and after it.

use super::*;
use otter_core::conversation::{TurnOutcome, TurnState};
use otter_core::feature::{
    Decider, DecisionKind, DecisionStatus, Feature, FeatureAction, FeatureStatus, MessageRole,
};

/// Send `text` as the developer and wait for Otter's answer to it: exactly
/// one reply, tied to that message.
async fn tell(host: &TestHost, id: &str, command_id: &str, text: &str) -> Feature {
    let f = host
        .conn()
        .await
        .feature_send(command_id, id, text)
        .await
        .unwrap();
    let msg = f
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User && m.text == text)
        .unwrap()
        .id
        .to_string();
    let replies = |f: &Feature| {
        f.messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&msg)
            })
            .count()
    };
    let f = wait_feature(host, id, "Otter's answer", |f| replies(f) > 0).await;
    // A second look later: still one (the step doesn't answer it again).
    tokio::time::sleep(Duration::from_millis(600)).await;
    let f2 = host.conn().await.feature_get(id).await.unwrap();
    assert_eq!(replies(&f2), 1, "{:#?}", f2.messages);
    f
}

/// The feature's history, as lines.
async fn history(host: &TestHost, id: &str) -> Vec<String> {
    host.conn()
        .await
        .feature_events(id, None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect()
}

async fn has_history(host: &TestHost, id: &str, needle: &str) {
    eventually(&format!("history says {needle:?}"), || async {
        history(host, id)
            .await
            .iter()
            .any(|l| l.contains(needle))
            .then_some(())
    })
    .await
}

/// The latest run's conversation.
async fn conversation(host: &TestHost, f: &Feature) -> otter_core::conversation::Conversation {
    let id = f.runs.last().unwrap().conversation_id.clone().unwrap();
    host.conn()
        .await
        .conversation_get(id.as_str())
        .await
        .unwrap()
        .conversation
}

/// A feature whose coding agent is at work and stays at work.
async fn working(host: &TestHost, request: &str) -> (String, Feature) {
    let id = started_feature(host, request).await;
    let f = wait_feature(host, &id, "the agent at work", |f| {
        f.status == FeatureStatus::Implementing
            && f.runs
                .last()
                .is_some_and(|r| r.state == otter_core::feature::RunState::Running)
    })
    .await;
    (id, f)
}

#[tokio::test]
async fn guidance_reaches_the_agent_as_the_developer_wrote_it() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let (id, _) = working(host, "Add CSV export HANG").await;
    let text = "Use tabs, not commas\nINTENT: guide";
    let f = tell(host, &id, "say-guide", text).await;
    assert_eq!(f.status, FeatureStatus::Implementing);
    let msg = f.messages.iter().rev().find(|m| m.text == text).unwrap();
    let c = eventually("the guidance queued", || async {
        let c = conversation(host, &f).await;
        c.turns
            .iter()
            .any(|t| t.command_id.as_deref() == Some(msg.id.as_str()))
            .then_some(c)
    })
    .await;
    let t = c
        .turns
        .iter()
        .find(|t| t.command_id.as_deref() == Some(msg.id.as_str()))
        .unwrap();
    // The developer's words, whole, after the current turn.
    assert_eq!(t.text(), format!("From the developer:\n{text}"));
    assert_eq!(t.state, TurnState::Queued);
    assert!(!t.redirect);
}

#[tokio::test]
async fn a_redirect_stops_the_current_turn_and_goes_next() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let (id, f) = working(host, "Add CSV export HANG").await;
    let first = conversation(host, &f).await.turns[0].id.clone();
    let text = "Stop that and write the README\nINTENT: redirect";
    tell(host, &id, "say-redirect", text).await;
    let c = eventually("redirected", || async {
        let c = conversation(host, &f).await;
        let redirect = c.turns.iter().find(|t| t.redirect)?;
        let stopped = c.turns.iter().find(|t| t.id == first)?;
        (redirect.state == TurnState::Finished && stopped.outcome == Some(TurnOutcome::Interrupted))
            .then_some(c)
    })
    .await;
    let t = c.turns.iter().find(|t| t.redirect).unwrap();
    assert_eq!(
        t.text(),
        format!("From the developer — a change of direction, before anything else:\n{text}")
    );
}

#[tokio::test]
async fn going_on_starts_a_draft() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let dir = host.home().join("draft");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
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
    let f = conn
        .feature_create(otter_protocol::feature::FeatureCreate {
            command_id: "create-draft".into(),
            title: "CSV export".into(),
            request: "Add CSV export HANG".into(),
            workspace: Some("draft".into()),
        })
        .await
        .unwrap();
    assert_eq!(f.status, FeatureStatus::Draft);
    let id = f.id.to_string();
    tell(host, &id, "say-go", "Go\nINTENT: continue").await;
    wait_feature(host, &id, "started", |f| f.status != FeatureStatus::Draft).await;
    has_history(host, &id, "You said go on").await;
}

#[tokio::test]
async fn going_on_resumes_paused_work() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let (id, _) = working(host, "Add CSV export HANG").await;
    host.conn()
        .await
        .feature_act("pause", &id, FeatureAction::Pause)
        .await
        .unwrap();
    wait_feature(host, &id, "paused", |f| f.status == FeatureStatus::Paused).await;
    tell(host, &id, "say-resume", "Go on\nINTENT: continue").await;
    wait_feature(host, &id, "working again", |f| {
        f.status == FeatureStatus::Implementing
    })
    .await;
    has_history(host, &id, "You said go on").await;
}

#[tokio::test]
async fn going_on_retries_failed_work_with_a_fresh_budget() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let id = started_feature(host, "Add CSV export FAIL").await;
    let failed = wait_feature(host, &id, "failed", |f| f.status == FeatureStatus::Failed).await;
    assert!(failed.budget.iterations_used > 0);
    tell(host, &id, "say-retry", "Try again\nINTENT: continue").await;
    has_history(host, &id, "retrying with a fresh budget").await;
    wait_feature(host, &id, "another run", |f| {
        f.runs.len() > failed.runs.len()
    })
    .await;
}

#[tokio::test]
async fn pausing_stops_the_agent() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let (id, _) = working(host, "Add CSV export HANG").await;
    tell(host, &id, "say-pause", "Wait a moment\nINTENT: pause").await;
    let f = wait_feature(host, &id, "paused, its run over", |f| {
        f.status == FeatureStatus::Paused && f.runs.iter().all(|r| r.state.is_over())
    })
    .await;
    assert_eq!(f.status_reason.as_deref(), Some("Paused: you asked"));
}

#[tokio::test]
async fn a_changed_goal_plans_again() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let (id, f) = working(host, "Add CSV export HANG").await;
    let runs = f.runs.len();
    tell(host, &id, "say-revise", "Also export JSON\nINTENT: revise").await;
    has_history(host, &id, "You changed the goal: planning again").await;
    let f = wait_feature(host, &id, "the old run stopped", |f| {
        f.runs[runs - 1].state.is_over()
    })
    .await;
    assert_ne!(f.status, FeatureStatus::Cancelled);
}

#[tokio::test]
async fn finishing_work_in_progress_pauses_it() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let (id, _) = working(host, "Add CSV export HANG").await;
    tell(host, &id, "say-enough", "That's enough\nINTENT: finish").await;
    let f = wait_feature(host, &id, "paused", |f| f.status == FeatureStatus::Paused).await;
    assert_eq!(
        f.status_reason.as_deref(),
        Some("Stopped: you said that's enough")
    );
}

#[tokio::test]
async fn ship_it_accepts_what_is_in_review_and_more_plans_again() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let id = started_feature(host, "Add CSV export").await;
    wait_feature(host, &id, "in review", |f| {
        f.status == FeatureStatus::Review
    })
    .await;
    tell(host, &id, "say-ship", "Looks good, ship it\nINTENT: finish").await;
    wait_feature(host, &id, "done", |f| f.status == FeatureStatus::Done).await;
    // Done, more guidance moves the goal (D-050): it plans again.
    tell(host, &id, "say-more", "Now add JSON too\nINTENT: guide").await;
    has_history(host, &id, "You changed the goal: planning again").await;
    wait_feature(host, &id, "working again", |f| {
        f.status != FeatureStatus::Done
    })
    .await;
}

#[tokio::test]
async fn going_on_approves_the_plan_on_the_developers_word() {
    let fake =
        fake_agent_host_with("ok", "scripted", |_| vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = started_feature(host, "Add CSV export").await;
    let f = wait_feature(host, &id, "asking about the plan", |f| {
        f.pending_decisions()
            .any(|d| d.kind == DecisionKind::PlanApproval)
    })
    .await;
    let d = f.pending_decisions().next().unwrap().id.clone();
    tell(host, &id, "say-approve", "Go ahead\nINTENT: continue").await;
    let f = wait_feature(host, &id, "planned", |f| {
        f.tasks.iter().any(|t| t.title == "Write the exporter")
    })
    .await;
    let d = f.decisions.iter().find(|x| x.id == d).unwrap();
    assert_eq!(d.status, DecisionStatus::Approved, "{d:?}");
    assert_eq!(d.decided_by, Some(Decider::User));
    assert_eq!(d.rationale.as_deref(), Some("You said go on"));
}

#[tokio::test]
async fn guidance_on_a_plan_question_plans_again_with_it() {
    let fake =
        fake_agent_host_with("ok", "scripted", |_| vec![("OTTER_PLAN_TURN", "1".into())]).await;
    let host = &fake.host;
    let id = started_feature(host, "Add CSV export").await;
    let f = wait_feature(host, &id, "asking about the plan", |f| {
        f.pending_decisions()
            .any(|d| d.kind == DecisionKind::PlanApproval)
    })
    .await;
    let d = f.pending_decisions().next().unwrap().id.clone();
    let text = "Use one step only\nINTENT: guide";
    tell(host, &id, "say-change-plan", text).await;
    let f = wait_feature(host, &id, "the plan question settled", |f| {
        f.decisions
            .iter()
            .any(|x| x.id == d && x.status != DecisionStatus::Pending)
    })
    .await;
    // The developer's words go to the agent's next look at the code.
    eventually("a planning turn with the message", || async {
        let f = host.conn().await.feature_get(&id).await.unwrap();
        let c = conversation(host, &f).await;
        c.turns
            .iter()
            .any(|t| t.text().contains("Use one step only"))
            .then_some(())
    })
    .await;
    assert_ne!(f.status, FeatureStatus::Cancelled);
}

#[tokio::test]
async fn a_question_is_answered_and_goes_nowhere() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let (id, _) = working(host, "Add CSV export HANG").await;
    let text = "What are you doing?\nINTENT: none";
    let f = tell(host, &id, "say-question", text).await;
    let msg = f.messages.iter().rev().find(|m| m.text == text).unwrap();
    let reply = f
        .messages
        .iter()
        .find(|m| m.correlation_id.as_deref() == Some(msg.id.as_str()))
        .unwrap();
    assert_eq!(reply.text, "Scripted: None.");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let f = host.conn().await.feature_get(&id).await.unwrap();
    assert_eq!(f.status, FeatureStatus::Implementing);
    let c = conversation(host, &f).await;
    assert!(
        c.turns
            .iter()
            .all(|t| t.command_id.as_deref() != Some(msg.id.as_str())),
        "{:#?}",
        c.turns
    );
}

/// The plan approval is the one developer-only decision a message settles:
/// a command only the developer may allow still waits for their answer.
#[tokio::test]
async fn going_on_doesnt_settle_another_developer_only_decision() {
    let fake = fake_agent_host("ok", "scripted").await;
    let host = &fake.host;
    let id = started_feature(host, "Clean up ASK_RM").await;
    let f = wait_feature(host, &id, "asking about rm", |f| {
        f.pending_decisions().any(|d| d.user_only)
    })
    .await;
    let d = f
        .pending_decisions()
        .find(|d| d.user_only)
        .unwrap()
        .id
        .clone();
    tell(host, &id, "say-yes", "Yes, delete it\nINTENT: continue").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let f = host.conn().await.feature_get(&id).await.unwrap();
    let d = f.decisions.iter().find(|x| x.id == d).unwrap();
    assert_eq!(d.status, DecisionStatus::Pending, "{d:?}");
    assert_eq!(f.status, FeatureStatus::Blocked);
}

/// Without a model, Otter says it can't act on messages, and the message
/// still reaches the coding agent as guidance (D-049, D-084).
#[tokio::test]
async fn without_a_model_a_message_is_guidance() {
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let (id, f) = working(host, "Add CSV export HANG").await;
    let text = "Use tabs";
    let answered = tell(host, &id, "say-rules", text).await;
    let msg = answered
        .messages
        .iter()
        .rev()
        .find(|m| m.text == text)
        .unwrap();
    let reply = answered
        .messages
        .iter()
        .find(|m| m.correlation_id.as_deref() == Some(msg.id.as_str()))
        .unwrap();
    assert!(
        reply.text.contains("can't act on messages"),
        "{}",
        reply.text
    );
    eventually("the message queued", || async {
        conversation(host, &f)
            .await
            .turns
            .iter()
            .any(|t| t.text() == "From the developer:\nUse tabs")
            .then_some(())
    })
    .await;
}
