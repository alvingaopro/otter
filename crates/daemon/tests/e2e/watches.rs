//! Watches (D-183) and what CI really says (D-184): Otter looks again
//! later, on the developer's word, and says once what it came to. Driven by
//! the `scripted` brain with `OTTER_BRAIN_V2=1` (a message's `CALL:` lines
//! are the model's tool calls) and the fake `gh`, a minute a second.

use super::*;
use otter_core::task::{GateStatus, MessageRole, Task, TaskAction, TaskEvent, TaskStatus};
use otter_core::watch::{Outcome, WatchStatus};
use serde_json::{Value, json};

async fn host_with(more: Vec<(&'static str, String)>) -> FakeCodex {
    fake_agent_host_with("ok", "scripted", move |_| {
        more.into_iter()
            .chain([
                ("OTTER_BRAIN_V2", "1".into()),
                ("OTTER_WATCH_TICK_MS", "100".into()),
                ("OTTER_WATCH_POLL_MS", "100".into()),
                ("OTTER_WATCH_MINUTE_MS", "1000".into()),
            ])
            .collect()
    })
    .await
}

/// A Git task published: its pull request open, CI not reported yet.
async fn published(fake: &FakeCodex) -> String {
    let (id, _) = git_task(fake).await;
    allow_publish(&fake.host, &id).await;
    wait_task(&fake.host, &id, "published, CI waited on", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
            && f.gates.iter().any(|g| g.name == "CI")
    })
    .await;
    id
}

/// Send `script` as the developer; wait for Otter's one reply to it. Each
/// call's answer, in order.
async fn say(host: &TestHost, id: &str, command_id: &str, script: &str) -> Vec<Value> {
    let f = host
        .conn()
        .await
        .task_send(command_id, id, script)
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
    let reply = |f: &Task| {
        f.messages
            .iter()
            .find(|m| {
                m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&msg)
            })
            .map(|m| m.text.clone())
    };
    let f = wait_task(host, id, "Otter's answer", |f| reply(f).is_some()).await;
    reply(&f)
        .unwrap()
        .lines()
        .filter_map(|l| {
            l.split_once(": ")
                .and_then(|(_, j)| serde_json::from_str(j).ok())
        })
        .collect()
}

fn code(answer: &Value) -> &str {
    answer["error"]["code"].as_str().unwrap_or("ok")
}

/// Otter's reports of watches: tied to the watch, never to a message.
fn reports(f: &Task) -> Vec<String> {
    f.messages
        .iter()
        .filter(|m| {
            m.role == MessageRole::Controller
                && m.correlation_id
                    .as_deref()
                    .is_some_and(|c| c.starts_with("watch:"))
        })
        .map(|m| m.text.clone())
        .collect()
}

fn checks(fake: &FakeCodex, states: &[(&str, &str)]) {
    let all: Vec<Value> = states
        .iter()
        .map(
            |(name, bucket)| json!({"name": name, "bucket": bucket, "link": "", "description": ""}),
        )
        .collect();
    write_checks(&fake.gh, "checks.json", json!(all));
}

const WATCH_CI: &str = r#"CALL: watch {"what": "tell me when CI is done", "kind": "ci"}"#;

#[tokio::test]
async fn a_ci_watch_says_once_what_ci_came_to() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = published(&fake).await;

    let answers = say(host, &id, "watch-1", WATCH_CI).await;
    assert_eq!(code(&answers[0]), "ok", "{answers:?}");
    assert_eq!(answers[0]["data"]["watching"], "CI on #7");
    // Read as it's set (W-1): the answer says what CI is now.
    assert_eq!(answers[0]["data"]["now"]["text"], "no checks yet");
    let f = host.conn().await.task_get(&id).await.unwrap();
    let w = &f.watches[0];
    assert_eq!((w.kind.as_str(), w.status), ("ci", WatchStatus::Waiting));
    assert_eq!(w.target["repo"], "o/r");
    assert!(w.asked_in.is_some());
    assert_eq!(w.readings.len(), 1, "{:?}", w.readings);
    // Asked again while it waits: the same watch.
    let again = say(host, &id, "watch-2", WATCH_CI).await;
    assert_eq!(again[0]["data"]["done"], "already watching", "{again:?}");

    // Still running: a reading, nothing said.
    checks(&fake, &[("test", "pending"), ("lint", "pass")]);
    wait_task(host, &id, "a reading", |f| {
        f.watches[0]
            .readings
            .iter()
            .any(|r| r.text.starts_with("1 of 2 checks running, 1 passed"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert!(reports(&f).is_empty(), "{:?}", reports(&f));
    // Read again and again, each change kept once: none yet, then running.
    let seen: Vec<&str> = f.watches[0]
        .readings
        .iter()
        .map(|r| r.text.as_str())
        .collect();
    assert_eq!(
        seen,
        [
            "no checks yet",
            "1 of 2 checks running, 1 passed (at 0123456)"
        ]
    );

    // Every check passed: said once, the watch ended.
    checks(&fake, &[("test", "pass"), ("lint", "pass")]);
    let f = wait_task(host, &id, "reported", |f| {
        f.watches[0].status == WatchStatus::Reported
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let f2 = host.conn().await.task_get(&id).await.unwrap();
    let said = reports(&f2);
    assert_eq!(said.len(), 1, "{said:#?}");
    assert!(
        said[0].starts_with("Watch: The developer asked you to watch CI on #7"),
        "{}",
        said[0]
    );
    assert!(
        said[0].contains("passed — all 2 checks passed"),
        "{}",
        said[0]
    );
    let w = &f.watches[0];
    assert_eq!(w.finding.as_ref().unwrap().outcome, Outcome::Passed);
    assert_eq!(
        w.report.as_ref(),
        f2.messages
            .iter()
            .rev()
            .find(|m| m.text == said[0])
            .map(|m| &m.id)
    );

    // Its history, in order; gh read with --repo; never a merge.
    let history: Vec<TaskEvent> = host
        .conn()
        .await
        .task_events(&id, None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.event)
        .filter(|e| {
            matches!(
                e,
                TaskEvent::WatchSet { .. }
                    | TaskEvent::WatchFiring { .. }
                    | TaskEvent::WatchReported { .. }
            )
        })
        .collect();
    assert_eq!(history.len(), 3, "{history:#?}");
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap();
    assert!(
        calls.contains("gh pr checks 7 --json name,bucket,link,description --repo o/r"),
        "{calls}"
    );
    assert!(!calls.contains("pr merge"), "{calls}");

    // Asked once CI has finished: it says what CI came to, and sets nothing.
    let answers = say(host, &id, "watch-3", WATCH_CI).await;
    assert_eq!(
        answers[0]["data"]["done"], "nothing to wait for",
        "{answers:?}"
    );
    assert_eq!(answers[0]["data"]["came_to"]["outcome"], "passed");
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert_eq!(f.watches.len(), 1, "{:?}", f.watches);
}

/// GitHub can't be read as the watch is set: it's set all the same, the
/// answer says why nothing was read, and it reads on its next look (W-1).
#[tokio::test]
async fn a_watch_that_cant_read_yet_says_so_and_is_set() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = published(&fake).await;
    std::fs::write(fake.gh.join("pr-view-fails"), "").unwrap();
    let answers = say(host, &id, "watch-1", WATCH_CI).await;
    assert_eq!(answers[0]["data"]["done"], "watching", "{answers:?}");
    assert!(answers[0]["data"]["now"].is_null(), "{answers:?}");
    assert!(
        answers[0]["data"]["not_read"]
            .as_str()
            .unwrap()
            .contains("error connecting"),
        "{answers:?}"
    );
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert!(f.watches[0].readings.is_empty(), "{:?}", f.watches);
    std::fs::remove_file(fake.gh.join("pr-view-fails")).unwrap();
    wait_task(host, &id, "read on its next look", |f| {
        f.watches[0].last_reading().is_some()
    })
    .await;
    // Asked again: that watch, with what it read.
    let again = say(host, &id, "watch-2", WATCH_CI).await;
    assert_eq!(again[0]["data"]["done"], "already watching", "{again:?}");
    assert_eq!(
        again[0]["data"]["now"]["text"], "no checks yet",
        "{again:?}"
    );
}

/// D-199: a pull request that conflicts with its base sends the task back
/// to work by itself — the coding agent brings the branch up to date — and
/// once it no longer conflicts and CI passed, it is done. Nothing waits on
/// the developer. The conflict read before the step isn't taken for the
/// answer: the pull request is read again first (here the agent leaves the
/// head as it was, so nothing is pushed to make it read at once, and it's
/// read every 2 s).
#[tokio::test]
async fn a_conflicting_pull_request_goes_back_to_work_and_ends_done() {
    let fake = host_with(vec![("OTTER_PR_POLL_MS", "2000".into())]).await;
    let host = &fake.host;
    std::fs::write(fake.gh.join("mergeable"), "MERGEABLE").unwrap();
    let id = published(&fake).await;
    // Its base moved on: now it conflicts.
    std::fs::write(fake.gh.join("mergeable"), "CONFLICTING").unwrap();
    let f = wait_task(host, &id, "back to work", |f| {
        f.steps
            .iter()
            .any(|t| t.title == "Bring the branch up to date")
    })
    .await;
    // Resolved meanwhile (here, by the forge), and CI passes.
    std::fs::write(fake.gh.join("mergeable"), "MERGEABLE").unwrap();
    checks(&fake, &[("test", "pass")]);
    let step = f
        .steps
        .iter()
        .find(|t| t.title == "Bring the branch up to date")
        .unwrap();
    assert!(
        step.detail
            .as_deref()
            .is_some_and(|d| d.contains("Merge origin/main into this branch")),
        "{step:?}"
    );
    let f = wait_task(host, &id, "done by itself", |f| {
        f.status == TaskStatus::Done
    })
    .await;
    assert_eq!(f.status_reason.as_deref(), Some("Done: every gate passed"));
    assert!(
        f.messages.iter().any(|m| m
            .text
            .contains("conflicts with main, so GitHub runs no CI on it: the coding agent brings")),
        "{:#?}",
        f.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    let step = f
        .steps
        .iter()
        .find(|t| t.title == "Bring the branch up to date")
        .unwrap();
    assert_eq!(step.status, otter_core::task::StepStatus::Done);
}

/// D-184: a pull request that conflicts gets no CI. That isn't "running":
/// the gates say so, Otter says so once; asked to watch it, Otter reads it
/// first and says so (W-1), and a watch that comes to it fires at once.
#[tokio::test]
async fn a_conflicting_pull_request_is_said_at_once() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    std::fs::write(fake.gh.join("mergeable"), "CONFLICTING").unwrap();
    let id = published(&fake).await;
    let f = wait_task(host, &id, "the gates say it", |f| {
        f.gates.iter().any(|g| {
            g.name == "CI"
                && g.status == GateStatus::Failed
                && g.detail.as_deref() == Some("can't run: the branch conflicts with main")
        })
    })
    .await;
    assert!(
        f.gates
            .iter()
            .any(|g| g.name == "Pull request" && g.status == GateStatus::Failed),
        "{:?}",
        f.gates
    );
    assert_eq!(
        f.delivery.as_ref().unwrap().mergeable.as_deref(),
        Some("CONFLICTING")
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    let f = host.conn().await.task_get(&id).await.unwrap();
    let said = f
        .messages
        .iter()
        .filter(|m| {
            m.text
                .contains("conflicts with main, so GitHub runs no CI on it: the coding agent brings the branch up to date")
        })
        .count();
    assert_eq!(said, 1, "once per head");

    // Asked to watch it as it is: read at once, nothing to wait for.
    let answers = say(host, &id, "watch-1", WATCH_CI).await;
    assert_eq!(
        answers[0]["data"]["done"], "nothing to wait for",
        "{answers:?}"
    );
    assert_eq!(answers[0]["data"]["came_to"]["outcome"], "stuck");
    assert!(
        answers[0]["data"]["came_to"]["text"]
            .as_str()
            .unwrap()
            .contains("conflicts with main"),
        "{answers:?}"
    );
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert!(f.watches.is_empty(), "{:?}", f.watches);

    // Watched while it could still run; then it conflicts: said at once.
    std::fs::write(fake.gh.join("mergeable"), "MERGEABLE").unwrap();
    let answers = say(host, &id, "watch-2", WATCH_CI).await;
    assert_eq!(answers[0]["data"]["done"], "watching", "{answers:?}");
    assert_eq!(answers[0]["data"]["now"]["text"], "no checks yet");
    std::fs::write(fake.gh.join("mergeable"), "CONFLICTING").unwrap();
    let f = wait_task(host, &id, "reported", |f| {
        f.watches
            .first()
            .is_some_and(|w| w.status == WatchStatus::Reported)
    })
    .await;
    let w = &f.watches[0];
    assert_eq!(w.finding.as_ref().unwrap().outcome, Outcome::Stuck);
    assert!(
        reports(&f)[0].contains("it conflicts with main, so GitHub runs no CI on it"),
        "{:?}",
        reports(&f)
    );
}

/// Accepted anyway before CI finished: CI is watched by itself, and what it
/// comes to is said on the done task.
#[tokio::test]
async fn accepting_before_ci_finishes_watches_it() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = published(&fake).await;
    wait_task(host, &id, "CI pending", |f| {
        f.gates
            .iter()
            .any(|g| g.name == "CI" && g.status == GateStatus::Pending)
    })
    .await;
    let f = host
        .conn()
        .await
        .task_act(
            "accept-anyway",
            &id,
            TaskAction::Accept {
                override_gates: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.status, TaskStatus::Done);
    let w = &f.watches[0];
    assert_eq!((w.kind.as_str(), w.status), ("ci", WatchStatus::Waiting));
    assert!(w.asked_in.is_none());
    assert!(
        f.messages.iter().any(|m| m
            .text
            .starts_with("CI on #7 hadn't finished when you accepted")),
        "{:#?}",
        f.messages
    );

    checks(&fake, &[("test", "fail"), ("lint", "pass")]);
    let f = wait_task(host, &id, "reported on the done task", |f| {
        f.watches[0].status == WatchStatus::Reported
    })
    .await;
    assert_eq!(f.status, TaskStatus::Done);
    assert_eq!(
        f.watches[0].finding.as_ref().unwrap().outcome,
        Outcome::Failed
    );
    assert!(
        reports(&f).iter().any(|r| r.contains("failed: test")),
        "{:?}",
        reports(&f)
    );
}

/// Fired, then otterd stopped before it said so: said once, after, on what
/// was found.
#[tokio::test]
async fn a_watch_that_fired_is_reported_once_after_a_crash() {
    let mut fake = host_with(vec![("OTTER_TEST_CRASH", "watch:report".into())]).await;
    let id = published(&fake).await;
    say(&fake.host, &id, "watch-1", WATCH_CI).await;
    checks(&fake, &[("test", "pass")]);
    // otterd stops right before the report.
    fake.host.revive_without("OTTER_TEST_CRASH").await;
    let f = wait_task(&fake.host, &id, "reported after the restart", |f| {
        f.watches[0].status == WatchStatus::Reported
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let f2 = fake.host.conn().await.task_get(&id).await.unwrap();
    assert_eq!(reports(&f2).len(), 1, "{:?}", reports(&f2));
    assert_eq!(f.watches[0].findings, 1, "fired once");
}

/// A time watch says the time came; a watch's own turn can't set another;
/// at most three wait; CI with no pull request isn't watched; cancelling
/// the task stops them all.
#[tokio::test]
async fn watches_are_bounded_and_end_with_their_task() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;

    let answers = say(
        host,
        &id,
        "soon",
        r#"CALL: watch {"what": "see how the agent is doing", "kind": "time", "in_minutes": 1}"#,
    )
    .await;
    assert_eq!(code(&answers[0]), "ok", "{answers:?}");
    let f = wait_task(host, &id, "the time came", |f| {
        f.watches
            .first()
            .is_some_and(|w| w.status == WatchStatus::Reported)
    })
    .await;
    assert_eq!(f.watches[0].finding.as_ref().unwrap().outcome, Outcome::Due);
    assert!(reports(&f)[0].starts_with("Watch:"), "{:?}", reports(&f));

    // No message started it: no watch where watching is the developer's
    // word (D-191).
    set_levels(
        host,
        &[("watch", "developer"), ("cancel_watch", "developer")],
    )
    .await;
    let answers = say(
        host,
        &id,
        "event",
        "AS: event\nCALL: watch {\"what\": \"x\", \"kind\": \"time\", \"in_minutes\": 5}",
    )
    .await;
    assert_eq!(code(&answers[0]), "not_allowed_in_this_turn", "{answers:?}");

    let answers = say(
        host,
        &id,
        "event-stop",
        "AS: event\nCALL: cancel_watch {\"watch_id\": \"wch_x\"}",
    )
    .await;
    assert_eq!(code(&answers[0]), "not_allowed_in_this_turn", "{answers:?}");
    set_levels(host, &[("watch", "auto"), ("cancel_watch", "auto")]).await;

    // Two looks in as many minutes, for different things, are two.
    let answers = say(
        host,
        &id,
        "many",
        &[
            (60, "is it stuck"),
            (60, "did the deploy finish"),
            (62, "c"),
            (63, "d"),
        ]
        .map(|(m, w)| {
            format!("CALL: watch {{\"what\": \"{w}\", \"kind\": \"time\", \"in_minutes\": {m}}}")
        })
        .join("\n"),
    )
    .await;
    let codes: Vec<&str> = answers.iter().map(code).collect();
    assert_eq!(codes, ["ok", "ok", "ok", "conflict"], "{answers:#?}");
    let answers = say(host, &id, "ci", WATCH_CI).await;
    assert_eq!(code(&answers[0]), "invalid", "no pull request: {answers:?}");

    let f = host
        .conn()
        .await
        .task_act("cancel", &id, TaskAction::Cancel)
        .await
        .unwrap();
    let open = f.watches.iter().filter(|w| w.status.is_open()).count();
    assert_eq!(open, 0, "{:#?}", f.watches);
    assert_eq!(
        f.watches
            .iter()
            .filter(|w| w.status == WatchStatus::Cancelled)
            .count(),
        3
    );
}

/// The developer stops and deletes a watch through the task's actions.
/// When a watch comes to something, its turn does what is automatic on
/// its own (D-193) — here it resumes the task the developer paused — but
/// sets no other watch (D-183).
#[tokio::test]
async fn a_watchs_turn_acts_on_its_own_and_sets_no_other_watch() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export HANG").await;
    wait_task(host, &id, "at work", |f| {
        f.status == TaskStatus::Implementing
    })
    .await;
    let answers = say(
        host,
        &id,
        "later",
        concat!(
            r#"CALL: watch {"what": "go on then\nCALL: resume_task {}\nCALL: watch {\"what\": \"again\", \"kind\": \"time\", \"in_minutes\": 1}", "kind": "time", "in_minutes": 1}"#,
            "\nCALL: pause_task {}"
        ),
    )
    .await;
    assert_eq!(
        answers.iter().map(code).collect::<Vec<_>>(),
        ["ok", "ok"],
        "{answers:#?}"
    );
    let f = wait_task(host, &id, "the watch's turn resumed it", |f| {
        f.watches
            .first()
            .is_some_and(|w| w.status == WatchStatus::Reported)
    })
    .await;
    let report = reports(&f).join("\n");
    assert!(report.contains("resume_task: {\"data\""), "{report}");
    assert!(
        report.contains("a watch's own turn doesn't set another watch"),
        "{report}"
    );
    assert_ne!(f.status, TaskStatus::Paused, "{:?}", f.status_reason);
    assert_eq!(f.watches.len(), 1, "no second watch");
    let recs = host.conn().await.task_events(&id, None).await.unwrap();
    assert!(recs.iter().any(|r| matches!(&r.event,
        otter_core::task::TaskEvent::ToolCalled { tool, origin, .. }
            if tool == "resume_task" && origin == "event")));
}

#[tokio::test]
async fn a_watch_is_stopped_and_deleted_by_the_developer() {
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    say(
        host,
        &id,
        "later",
        r#"CALL: watch {"what": "look again", "kind": "time", "in_minutes": 90}"#,
    )
    .await;
    let f = host.conn().await.task_get(&id).await.unwrap();
    let wid = f.watches[0].id.clone();
    let mut conn = host.conn().await;
    let f = conn
        .task_act(
            "stop",
            &id,
            TaskAction::CancelWatch {
                watch_id: wid.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(f.watches[0].status, WatchStatus::Cancelled);
    let err = conn
        .task_act(
            "stop-again",
            &id,
            TaskAction::CancelWatch {
                watch_id: wid.clone(),
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("already ended"), "{err:#}");
    let f = conn
        .task_act("delete", &id, TaskAction::DeleteWatch { watch_id: wid })
        .await
        .unwrap();
    assert!(f.watches.is_empty());
}

/// A trigger is paused and resumed (D-185); clients hear of each change.
#[tokio::test]
async fn a_trigger_is_paused_and_resumed() {
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    plain_workspace(host, "tidy").await;
    let mut conn = host.conn().await;
    let mut create = trigger("tidy", "tidy", "Tidy up", Default::default());
    create.schedule = otter_core::trigger::Schedule::Every { minutes: 60 };
    conn.trigger_create(create).await.unwrap();
    let t = conn.trigger_update("tidy", Some(false)).await.unwrap();
    assert!(!t.enabled);
    let t = conn.trigger_update("tidy", Some(true)).await.unwrap();
    assert!(t.enabled && t.next_at.is_some());
    let changed = conn
        .events_list(Some(50))
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e.event, otter_protocol::Event::TriggersChanged { .. }))
        .count();
    assert_eq!(changed, 3, "made, paused, resumed");
}

/// No check on the head long after the watch first saw it: said, not
/// waited on to the deadline (D-184).
#[tokio::test]
async fn a_ci_watch_says_when_no_check_starts() {
    let fake = host_with(vec![("OTTER_CI_START_MS", "1500".into())]).await;
    let host = &fake.host;
    let id = published(&fake).await;
    say(host, &id, "watch-1", WATCH_CI).await;
    let f = wait_task(host, &id, "reported", |f| {
        f.watches
            .first()
            .is_some_and(|w| w.status == WatchStatus::Reported)
    })
    .await;
    let w = &f.watches[0];
    assert_eq!(w.finding.as_ref().unwrap().outcome, Outcome::Stuck);
    assert!(
        w.finding
            .as_ref()
            .unwrap()
            .text
            .starts_with("no CI check has started on it"),
        "{:?}",
        w.finding
    );
    assert_eq!(w.readings[0].text, "no checks yet");
}

/// While a watch's turn looks at what it found, the app is told Otter is at
/// it; once the report is written, that it stopped (D-196).
#[tokio::test]
async fn otter_is_shown_at_it_while_a_watch_is_reported() {
    use otter_protocol::Event;
    let fake = host_with(vec![]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    let mut events = host.conn().await.subscribe(None).await.unwrap();
    let answers = say(
        host,
        &id,
        "soon",
        r#"CALL: watch {"what": "see how the agent is doing", "kind": "time", "in_minutes": 1}"#,
    )
    .await;
    assert_eq!(code(&answers[0]), "ok", "{answers:?}");
    let mut said = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let rec = events.next().await.unwrap().unwrap();
            if let Event::TaskStream {
                stream_id,
                text,
                done,
                ..
            } = rec.event
                && stream_id.starts_with("watch-")
            {
                said.push((text, done));
                if done {
                    return;
                }
            }
        }
    })
    .await
    .expect("the watch's turn ends");
    assert_eq!(said, [(String::new(), false), (String::new(), true)]);
    // Its report is in the conversation by then.
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert!(
        f.watches
            .first()
            .is_some_and(|w| w.status == WatchStatus::Reported),
        "{:?}",
        f.watches
    );
}
