//! Otter's conversation on a host (D-093, plan M3): about any task there,
//! with the host's tools — looking across tasks, making, starting, pausing
//! them — answered once per message, and kept across a restart.

use super::*;
use otter_core::task::{MessageRole, OtterChat, TaskEvent, TaskStatus};
use serde_json::Value;

async fn chat_host(v2: bool) -> FakeCodex {
    fake_agent_host_with("ok", "scripted", move |_| {
        vec![("OTTER_BRAIN_V2", if v2 { "1" } else { "0" }.into())]
    })
    .await
}

/// Say `text` to Otter on the host; wait for its one reply. The chat then,
/// the reply, and each tool call's answer in order.
pub(super) async fn say(
    host: &TestHost,
    command_id: &str,
    text: &str,
) -> (OtterChat, String, Vec<Value>) {
    let chat = host
        .conn()
        .await
        .otter_send(command_id, text)
        .await
        .unwrap();
    let msg = chat
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User && m.text == text)
        .unwrap()
        .id
        .to_string();
    let replies = |c: &OtterChat| -> Vec<String> {
        c.messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&msg)
            })
            .map(|m| m.text.clone())
            .collect()
    };
    eventually("Otter's answer on the host", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        (!replies(&c).is_empty()).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let chat = host.conn().await.otter_get().await.unwrap();
    let all = replies(&chat);
    assert_eq!(all.len(), 1, "one reply: {all:#?}");
    let answers = all[0]
        .lines()
        .filter_map(|l| {
            l.split_once(": ")
                .and_then(|(_, j)| serde_json::from_str(j).ok())
        })
        .collect();
    (chat, all[0].clone(), answers)
}

fn code(answer: &Value) -> &str {
    answer["error"]["code"].as_str().unwrap_or("ok")
}

/// From Otter's conversation on the host: look at the tasks, make one in the
/// developer's words and start it, and it is there after a restart (plan M3's
/// exit).
#[tokio::test]
async fn otter_on_the_host_looks_across_tasks_and_makes_one() {
    let mut fake = chat_host(true).await;
    let host = &fake.host;
    let first = started_task(host, "Add CSV export HANG").await;
    wait_task(host, &first, "at work", |f| {
        f.status == TaskStatus::Implementing
    })
    .await;
    let (_, _, answers) = say(
        host,
        "h-1",
        &format!(
            "What's going on?\nCALL: list_tasks {{}}\nCALL: get_task {{\"task_id\": \"{first}\"}}"
        ),
    )
    .await;
    let tasks = answers[0]["data"]["newest_first"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "{answers:?}");
    assert_eq!(tasks[0]["id"], first.as_str());
    assert_eq!(tasks[0]["workspace"], "proj");
    assert_eq!(answers[1]["data"]["status"], "implementing", "{answers:?}");

    let text = "The login form forgets the user's email: fix it in proj, and start on it.\nCALL: create_task {\"title\": \"Fix the login form\", \"message_ids\": $MSG, \"workspace\": \"proj\", \"start\": true}";
    let (chat, _, answers) = say(host, "h-2", text).await;
    assert_eq!(answers[0]["data"]["done"], "created", "{answers:?}");
    let made = answers[0]["data"]["task_id"].as_str().unwrap().to_owned();
    let f = host.conn().await.task_get(&made).await.unwrap();
    // The request is the developer's words, as written; the title Otter's.
    assert_eq!(f.request, text);
    assert_eq!(f.title, "Fix the login form");
    assert!(f.workspace_id.is_some());
    assert_ne!(f.status, TaskStatus::Draft, "started");
    // Written down in the conversation's history, under the message.
    let msg = chat
        .messages
        .iter()
        .rev()
        .find(|m| m.text == text)
        .unwrap()
        .id
        .to_string();
    let history = host.conn().await.otter_events(None).await.unwrap();
    assert!(history.iter().any(|r| matches!(
        &r.event,
        TaskEvent::ToolCalled { tool, authority, .. }
            if tool == "create_task" && authority.as_deref() == Some(msg.as_str())
    )));
    // Kept across a restart.
    let before = chat.messages.len();
    fake.host.restart().await;
    let chat = fake.host.conn().await.otter_get().await.unwrap();
    assert_eq!(chat.messages.len(), before);
}

/// A turn an event started can pause a task; making one is the
/// developer's word where that is its level (D-191).
#[tokio::test]
async fn otter_on_the_host_acts_on_the_developers_word_only() {
    let fake = chat_host(true).await;
    let host = &fake.host;
    set_levels(host, &[("create_task", "developer")]).await;
    let first = started_task(host, "Add CSV export HANG").await;
    wait_task(host, &first, "at work", |f| {
        f.status == TaskStatus::Implementing
    })
    .await;
    let (_, _, answers) = say(
        host,
        "h-ev",
        &format!(
            "AS: event\nCALL: create_task {{\"title\": \"x\", \"message_ids\": $MSG}}\nCALL: pause_task {{\"task_id\": \"{first}\"}}"
        ),
    )
    .await;
    assert_eq!(code(&answers[0]), "not_allowed_in_this_turn", "{answers:?}");
    assert_eq!(code(&answers[1]), "ok", "{answers:?}");
    wait_task(host, &first, "paused", |f| f.status == TaskStatus::Paused).await;
    assert_eq!(host.conn().await.task_list().await.unwrap().len(), 1);
    // A task Otter can't find is said so.
    let (_, _, answers) = say(host, "h-none", "CALL: get_task {\"task_id\": \"ft_nope\"}").await;
    assert_eq!(code(&answers[0]), "not_found", "{answers:?}");
}

/// Without Otter's tools, the host conversation says how to have them.
#[tokio::test]
async fn otter_on_the_host_without_tools_says_so() {
    let fake = chat_host(false).await;
    let (_, reply, _) = say(&fake.host, "h-off", "What's going on?").await;
    assert!(reply.contains("Otter's tools"), "{reply}");
    assert!(reply.contains("OTTER_BRAIN_V2=0"), "{reply}");
}

/// Real Claude as Otter on the host, through the built worker (D-093): asked
/// to make a task in a workspace and not start it, it makes a draft there
/// whose request is the developer's message as written. Run by hand:
/// `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime cargo test -p otterd
/// --test e2e live_otter_on_the_host -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
async fn live_otter_on_the_host_makes_a_task() {
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
    let dir = host.home().join("notes");
    std::fs::create_dir_all(&dir).unwrap();
    host.conn()
        .await
        .workspace_create(WorkspaceCreate {
            name: "notes".into(),
            source: SourceSpec::Directory {
                path: dir.to_string_lossy().into_owned(),
            },
            sessions: Some(vec![]),
            ..Default::default()
        })
        .await
        .unwrap();
    let text =
        "Make a task in the notes workspace: add a README that says hello. Don't start it yet.";
    let (_, reply, _) = say(&host, "live-1", text).await;
    println!("reply: {reply}");
    let tasks = host.conn().await.task_list().await.unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:#?}");
    let f = &tasks[0];
    println!("task: {} — {:?}", f.title, f.status);
    assert_eq!(f.request, text, "the developer's words");
    assert_eq!(f.status, TaskStatus::Draft, "not started");
    assert!(f.workspace_id.is_some(), "in the workspace they named");
    let (_, reply, _) = say(&host, "live-2", "Which tasks are here?").await;
    println!("reply: {reply}");
    assert!(
        reply.contains(&f.title) || reply.contains(f.id.as_str()),
        "{reply}"
    );
}

/// A project workspace `name` (a Makefile whose check passes), no task.
async fn project(host: &TestHost, name: &str) {
    let dir = host.home().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    host.conn()
        .await
        .workspace_create(WorkspaceCreate {
            name: name.into(),
            source: SourceSpec::Directory {
                path: dir.to_string_lossy().into_owned(),
            },
            sessions: Some(vec![]),
            ..Default::default()
        })
        .await
        .unwrap();
}

/// The follow-ups in the conversation (D-094), oldest first.
fn follow_ups(chat: &OtterChat) -> Vec<String> {
    chat.messages
        .iter()
        .filter(|m| {
            m.role == MessageRole::Controller
                && m.correlation_id
                    .as_deref()
                    .is_some_and(|c| c.starts_with("follow:"))
        })
        .map(|m| m.text.clone())
        .collect()
}

/// Otter follows up what it made (D-094), once per change: in review, then
/// done. A task it didn't make, it doesn't.
#[tokio::test]
async fn otter_follows_up_the_tasks_it_made_once_per_change() {
    use otter_core::task::TaskAction;
    let fake = chat_host(true).await;
    let host = &fake.host;
    project(host, "app").await;
    let other = started_task(host, "Add a footer").await;
    let (_, _, answers) = say(
        host,
        "f-1",
        "Add CSV export to app, and start it.\nCALL: create_task {\"title\": \"CSV export\", \"message_ids\": $MSG, \"workspace\": \"app\", \"start\": true}",
    )
    .await;
    let made = answers[0]["data"]["task_id"].as_str().unwrap().to_owned();
    assert_eq!(
        host.conn().await.otter_get().await.unwrap().follows.len(),
        1
    );
    // Delivered, the task is done by itself (D-199): that's the change
    // followed up (review isn't one any more).
    let chat = eventually("a follow-up on done", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        (!follow_ups(&c).is_empty()).then_some(c)
    })
    .await;
    let first = follow_ups(&chat);
    assert_eq!(first.len(), 1, "{first:#?}");
    assert!(
        first[0].starts_with(&format!("Follow-up: [{made}] CSV export is now done")),
        "{}",
        first[0]
    );
    // Its own look at the task, in a turn no message started.
    assert!(first[0].contains("get_task:"), "{}", first[0]);
    // No more for the same change, however often otterd wakes.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        follow_ups(&host.conn().await.otter_get().await.unwrap()).len(),
        1
    );
    // The next change has its own: more asked of it, done again.
    host.conn()
        .await
        .task_act(
            "more-made",
            &made,
            TaskAction::RequestChanges {
                note: Some("And JSON".into()),
            },
        )
        .await
        .unwrap();
    let chat = eventually("a follow-up on done again", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        (follow_ups(&c).len() == 2).then_some(c)
    })
    .await;
    assert!(
        follow_ups(&chat)[1].contains("is now done"),
        "{:#?}",
        follow_ups(&chat)
    );
    // The task it didn't make was delivered too, unremarked.
    wait_task(host, &other, "in review", delivered).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let chat = host.conn().await.otter_get().await.unwrap();
    assert!(
        follow_ups(&chat).iter().all(|m| !m.contains(&other)),
        "{:#?}",
        follow_ups(&chat)
    );
}

/// otterd dies after a follow-up's turn, before its message is kept (D-094):
/// after the restart it is posted once — not lost, not twice.
#[tokio::test]
async fn a_crash_before_a_follow_up_posts_it_once_after() {
    let mut fake = fake_agent_host_with("ok", "scripted", |_| {
        vec![
            ("OTTER_BRAIN_V2", "1".into()),
            ("OTTER_TEST_CRASH", "follow:post".into()),
        ]
    })
    .await;
    project(&fake.host, "app").await;
    // Sent, not waited on: otterd dies soon after it's answered.
    fake.host
        .conn()
        .await
        .otter_send(
            "f-crash",
            "Add CSV export to app, and start it.\nCALL: create_task {\"title\": \"CSV export\", \"message_ids\": $MSG, \"workspace\": \"app\", \"start\": true}",
        )
        .await
        .unwrap();
    eventually("otterd dies at the follow-up", || {
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
    eventually("the follow-up after the restart", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        (!follow_ups(&c).is_empty()).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let chat = host.conn().await.otter_get().await.unwrap();
    assert_eq!(follow_ups(&chat).len(), 1, "{:#?}", follow_ups(&chat));
}

/// Real Claude all the way (D-094): Otter on the host makes and starts a
/// small task, the coding agent does it, and Otter follows up once when it
/// is ready for review. Run by hand: `OTTER_CLAUDE_WORKER=$PWD/packages/claude-runtime
/// cargo test -p otterd --test e2e live_otter_follows_up -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "calls Claude; needs the built worker and a signed-in Claude Code"]
async fn live_otter_follows_up_a_task_it_made() {
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
        ("OTTER_CONTROLLER", "claude".into()),
        ("OTTER_CONTROLLER_MODEL", "haiku".into()),
        ("OTTER_BRAIN_V2", "1".into()),
        ("OTTER_CODING_BACKEND", "sdk".into()),
        ("OTTER_CLAUDE_WORKER", worker),
        ("OTTER_NODE", node),
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
    project(&host, "notes").await;
    let (_, reply, _) = say(
        &host,
        "live-f",
        "In the notes workspace, create hello.txt containing the word hi. Start it now.",
    )
    .await;
    println!("reply: {reply}");
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let chat = loop {
        let c = host.conn().await.otter_get().await.unwrap();
        if !follow_ups(&c).is_empty() {
            break c;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no follow-up: {:#?}",
            host.conn().await.task_list().await.unwrap()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    for f in follow_ups(&chat) {
        println!("follow-up: {f}");
    }
    let tasks = host.conn().await.task_list().await.unwrap();
    println!(
        "task: {} — {:?} ({:?})",
        tasks[0].title, tasks[0].status, tasks[0].status_reason
    );
    assert_eq!(follow_ups(&chat).len(), 1);
    assert!(
        host.home().join("notes/hello.txt").exists(),
        "the work was done"
    );
}

/// One message that asks for several things done apart is a task each
/// (D-205): each made once, its request the developer's words, its part of
/// them told to its coding agent as Otter's — the same call again is the
/// same task.
#[tokio::test]
async fn one_message_makes_a_task_for_each_part_it_asks_for() {
    use otter_core::task::RunState;
    let fake = chat_host(true).await;
    let host = &fake.host;
    let call = |title: &str, scope: &str| {
        format!(
            "CALL: create_task {{\"title\": \"{title}\", \"message_ids\": $MSG, \"scratch\": true, \"scope\": \"{scope}\", \"start\": true}}"
        )
    };
    let text = format!(
        "Review pull requests A and B HANG, a task each.\n{}\n{}\n{}",
        call("Review A", "Only A; don't approve it."),
        call("Review B", "Only B; don't approve it."),
        call("Review A", "Only A; don't approve it."),
    );
    let (_, _, answers) = say(host, "h-split", &text).await;
    let done: Vec<&str> = answers
        .iter()
        .map(|a| a["data"]["done"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(done, ["created", "created", "already"], "{answers:#?}");
    let id = |i: usize| answers[i]["data"]["task_id"].as_str().unwrap().to_owned();
    let (a, b) = (id(0), id(1));
    assert_ne!(a, b);
    assert_eq!(id(2), a, "the same call, the same task");
    assert_eq!(host.conn().await.task_list().await.unwrap().len(), 2);
    for (task, mine, other) in [(&a, "Only A", "Only B"), (&b, "Only B", "Only A")] {
        let f = wait_task(host, task, "its coding agent at work", |f| {
            f.runs.last().is_some_and(|r| r.state == RunState::Running)
        })
        .await;
        // The request is the developer's words; the part is Otter's.
        assert_eq!(f.request, text);
        let told: Vec<&str> = f
            .messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Controller
                    && m.correlation_id
                        .as_deref()
                        .is_some_and(|c| c.starts_with("tell:"))
            })
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(
            told,
            [format!("To the coding agent: {mine}; don't approve it.")]
        );
        // Its coding agent is told it, as Otter's, from its first turn.
        let conv = host
            .conn()
            .await
            .conversation_get(
                f.runs
                    .last()
                    .unwrap()
                    .conversation_id
                    .as_ref()
                    .unwrap()
                    .as_str(),
            )
            .await
            .unwrap();
        let first = conv
            .conversation
            .turns
            .first()
            .expect("a first turn")
            .text();
        let (_, otters) = first
            .split_once("From Otter, your tech lead (not the developer):\n")
            .expect(&first);
        assert!(otters.starts_with(&format!("- {mine}")), "{first}");
        assert!(!otters.contains(other), "{first}");
    }
}

/// A pull request reviewed on the developer's word (D-205): approved once
/// per message, a comment with what it says, not from a turn no message
/// started at its level (`developer`); GitHub's refusal is said, and asked
/// again it is tried again.
#[tokio::test]
async fn otter_reviews_a_pull_request_on_the_developers_word() {
    let fake = chat_host(true).await;
    let host = &fake.host;
    const PR: &str = "https://github.com/o/r/pull/7";
    let review = |event: &str, body: Option<&str>| {
        let body = body
            .map(|b| format!(", \"body\": \"{b}\""))
            .unwrap_or_default();
        format!("CALL: review_pull_request {{\"url\": \"{PR}\", \"event\": \"{event}\"{body}}}")
    };
    let text = [
        format!("Approve {PR}, and say it is small."),
        review("approve", None),
        review("approve", None),
        review("comment", None),
        review("comment", Some("Nice and small.")),
        "CALL: review_pull_request {\"url\": \"https://github.com/o/r/actions/runs/1\", \"event\": \"approve\"}".into(),
    ]
    .join("\n");
    let (_, _, answers) = say(host, "h-review", &text).await;
    assert_eq!(answers[0]["data"]["done"], "approved", "{answers:#?}");
    assert_eq!(answers[0]["data"]["pull_request"]["repo"], "o/r");
    assert_eq!(answers[0]["data"]["pull_request"]["number"], 7);
    assert_eq!(answers[1]["data"]["done"], "already", "{answers:#?}");
    assert_eq!(code(&answers[2]), "invalid", "a comment says something");
    assert_eq!(answers[3]["data"]["done"], "commented", "{answers:#?}");
    assert_eq!(code(&answers[4]), "invalid", "not a pull request");
    let reviews = std::fs::read_to_string(fake.gh.join("reviews")).unwrap();
    assert_eq!(
        reviews.lines().collect::<Vec<_>>(),
        [
            "pr review 7 --approve --repo o/r",
            "pr review 7 --comment --body-file - --repo o/r"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(fake.gh.join("review-body")).unwrap(),
        "Nice and small."
    );
    // Not on Otter's own.
    let (_, _, answers) = say(
        host,
        "h-review-ev",
        &format!("AS: event\n{}", review("approve", None)),
    )
    .await;
    assert_eq!(
        code(&answers[0]),
        "not_allowed_in_this_turn",
        "{answers:#?}"
    );
    // GitHub's no is said, and nothing is kept as done.
    std::fs::write(fake.gh.join("own-pr"), "").unwrap();
    let text = format!(
        "Approve it again.\n{}\n{}",
        review("approve", None),
        review("approve", None)
    );
    let (_, _, answers) = say(host, "h-review-own", &text).await;
    for a in &answers {
        assert_eq!(code(a), "conflict", "{answers:#?}");
        assert!(
            a["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Can not approve your own pull request"),
            "{a}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(fake.gh.join("reviews"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

/// What a task asks the developer, answered from the host's conversation as
/// their message says (D-205): a plan approved once, in their name; never
/// from a turn no message started, whatever the level.
#[tokio::test]
async fn otter_answers_a_tasks_decision_as_the_developer_said() {
    use otter_core::task::{Decider, DecisionKind, DecisionStatus};
    let fake = fake_agent_host_with("ok", "scripted", |_| {
        vec![
            ("OTTER_BRAIN_V2", "1".into()),
            // Planning asks the developer to approve the plan.
            ("OTTER_PLAN_TURN", "1".into()),
        ]
    })
    .await;
    let host = &fake.host;
    let id = started_task_named(host, "Add JSON export").await;
    let f = wait_task(host, &id, "asking about its plan", |f| {
        f.pending_decisions()
            .any(|d| d.kind == DecisionKind::PlanApproval)
    })
    .await;
    let dec = f.pending_decisions().next().unwrap().id.to_string();
    let answer = |more: &str| {
        format!("CALL: answer_decision {{\"task_id\": \"{id}\", \"decision_id\": \"{dec}\"{more}}}")
    };
    let (_, _, answers) = say(
        host,
        "h-dec-ev",
        &format!("AS: event\n{}", answer(", \"approve\": true")),
    )
    .await;
    assert_eq!(
        code(&answers[0]),
        "not_allowed_in_this_turn",
        "{answers:#?}"
    );
    // Whatever its level: only a message of the developer's answers.
    set_levels(host, &[("answer_decision", "auto")]).await;
    let (_, _, answers) = say(
        host,
        "h-dec-ev-auto",
        &format!("AS: event\n{}", answer(", \"approve\": true")),
    )
    .await;
    assert_eq!(code(&answers[0]), "needs_developer", "{answers:#?}");
    let text = format!(
        "Go ahead with that plan.\nCALL: get_pending_decisions {{\"task_id\": \"{id}\"}}\n{}\n{}\n{}",
        answer(", \"answer\": \"yes\""),
        answer(", \"approve\": true"),
        answer(", \"approve\": true"),
    );
    let (_, _, answers) = say(host, "h-dec", &text).await;
    // A plan put to the developer is theirs alone, and their message
    // answers it: Otter is shown so.
    let shown = &answers[0]["data"]["decisions"][0];
    assert_eq!(shown["developer_only"], true, "{shown}");
    assert_eq!(shown["answerable_by_message"], true, "{shown}");
    assert_eq!(code(&answers[1]), "invalid", "a plan takes no answer");
    assert_eq!(answers[2]["data"]["done"], "approved", "{answers:#?}");
    assert_eq!(answers[3]["data"]["done"], "already", "{answers:#?}");
    let f = host.conn().await.task_get(&id).await.unwrap();
    let d = f.decisions.iter().find(|d| d.id.as_str() == dec).unwrap();
    assert_eq!(d.status, DecisionStatus::Approved);
    assert_eq!(d.decided_by, Some(Decider::User));
    // Decided: said so.
    let (_, _, answers) = say(
        host,
        "h-dec-2",
        &format!("No, deny it.\n{}", answer(", \"approve\": false")),
    )
    .await;
    assert_eq!(code(&answers[0]), "conflict", "{answers:#?}");
}

/// A command only the developer may allow is answered by their own press,
/// not by a message to Otter (D-205, D-089 §3.2).
#[tokio::test]
async fn a_command_only_the_developer_may_allow_takes_their_press() {
    use otter_core::task::DecisionKind;
    let fake = chat_host(true).await;
    let host = &fake.host;
    let rm = started_task(host, "Clean up ASK_RM").await;
    let mine = |d: &&otter_core::task::DecisionRequest| {
        d.kind == DecisionKind::ToolPermission && d.user_only
    };
    let f = wait_task(host, &rm, "asking about rm", |f| {
        f.pending_decisions().any(|d| mine(&d))
    })
    .await;
    let d = f.pending_decisions().find(mine).unwrap();
    let (_, _, answers) = say(
        host,
        "h-dec-rm",
        &format!(
            "Yes, allow it.\nCALL: get_pending_decisions {{\"task_id\": \"{rm}\"}}\nCALL: answer_decision {{\"task_id\": \"{rm}\", \"decision_id\": \"{}\", \"approve\": true}}",
            d.id
        ),
    )
    .await;
    let shown = answers[0]["data"]["decisions"].as_array().unwrap();
    let shown = shown.iter().find(|x| x["id"] == d.id.as_str()).unwrap();
    assert_eq!(shown["answerable_by_message"], false, "{shown}");
    assert_eq!(code(&answers[1]), "needs_developer", "{answers:#?}");
    let f = host.conn().await.task_get(&rm).await.unwrap();
    assert!(f.pending_decisions().any(|x| x.id == d.id), "still waits");
}

/// A host whose coding agent is the fake SDK worker in `mode`
/// (`tests/fake_claude_worker.sh`).
async fn sdk_host(mode: &str) -> FakeCodex {
    let mode = mode.to_owned();
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
    })
    .await
}

/// The coding agent's question, answered from the host's conversation with
/// the developer's words (D-205); a form of several questions is theirs to
/// answer in its own form, and Otter is shown which is which.
#[tokio::test]
async fn otter_answers_a_question_and_leaves_a_form_to_the_developer() {
    use otter_core::task::{Decider, DecisionKind, DecisionStatus, Task};
    let asks = |f: &Task| {
        f.pending_decisions()
            .any(|d| d.kind == DecisionKind::Question)
    };
    let script = |id: &str, d: &str, answer: &str| {
        format!(
            "CALL: get_pending_decisions {{\"task_id\": \"{id}\"}}\nCALL: answer_decision {{\"task_id\": \"{id}\", \"decision_id\": \"{d}\", \"answer\": \"{answer}\"}}"
        )
    };

    // One question, no choices: answered as the developer said.
    let fake = sdk_host("onequestion").await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    let f = wait_task(host, &id, "asking which repository", asks).await;
    let d = f.pending_decisions().next().unwrap().id.to_string();
    let text = format!(
        "It is sugerio/marketplace-service.\n{}",
        script(&id, &d, "sugerio/marketplace-service")
    );
    let (_, _, answers) = say(host, "h-q", &text).await;
    assert_eq!(
        answers[0]["data"]["decisions"][0]["answerable_by_message"], true,
        "{answers:#?}"
    );
    assert_eq!(answers[1]["data"]["done"], "answered", "{answers:#?}");
    let f = host.conn().await.task_get(&id).await.unwrap();
    let q = f.decisions.iter().find(|x| x.id.as_str() == d).unwrap();
    assert_eq!(q.status, DecisionStatus::Answered);
    assert_eq!(q.answer.as_deref(), Some("sugerio/marketplace-service"));
    assert_eq!(q.decided_by, Some(Decider::User));

    // A form of several questions: theirs, in its own form.
    let fake = sdk_host("ok").await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    let f = wait_task(host, &id, "asking its form", asks).await;
    let d = f.pending_decisions().next().unwrap().id.to_string();
    let (_, _, answers) = say(host, "h-form", &format!("CSV.\n{}", script(&id, &d, "CSV"))).await;
    assert_eq!(
        answers[0]["data"]["decisions"][0]["answerable_by_message"], false,
        "{answers:#?}"
    );
    assert_eq!(code(&answers[1]), "needs_developer", "{answers:#?}");
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert!(
        f.pending_decisions().any(|x| x.id.as_str() == d),
        "still waits"
    );
}

/// A task stopped for good and deleted on the developer's word (D-207):
/// never from a turn no message started, whatever the level; deleted only
/// once nothing is at work in it; deleted again, gone already.
#[tokio::test]
async fn otter_stops_and_deletes_a_task_on_the_developers_word() {
    use otter_core::task::{RunState, TaskStatus};
    let fake = chat_host(true).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export HANG").await;
    wait_task(host, &id, "its coding agent at work", |f| {
        f.runs.last().is_some_and(|r| r.state == RunState::Running)
    })
    .await;
    let call = |tool: &str| format!("CALL: {tool} {{\"task_id\": \"{id}\"}}");
    // A turn no message started: refused, also at `auto`.
    let (_, _, answers) = say(
        host,
        "h-end-ev",
        &format!("AS: event\n{}", call("cancel_task")),
    )
    .await;
    assert_eq!(
        code(&answers[0]),
        "not_allowed_in_this_turn",
        "{answers:#?}"
    );
    set_levels(host, &[("cancel_task", "auto"), ("delete_task", "auto")]).await;
    let (_, _, answers) = say(
        host,
        "h-end-ev-auto",
        &format!(
            "AS: event\n{}\n{}",
            call("cancel_task"),
            call("delete_task")
        ),
    )
    .await;
    assert_eq!(code(&answers[0]), "needs_developer", "{answers:#?}");
    assert_eq!(code(&answers[1]), "needs_developer", "{answers:#?}");
    assert!(
        host.conn()
            .await
            .task_get(&id)
            .await
            .unwrap()
            .status
            .is_active()
    );
    // At work: not deleted, and said why.
    let (_, _, answers) = say(
        host,
        "h-del-early",
        &format!("Delete it.\n{}", call("delete_task")),
    )
    .await;
    assert_eq!(code(&answers[0]), "not_ready", "{answers:#?}");
    // Their word: stopped, then deleted, then gone already.
    let (_, _, answers) = say(
        host,
        "h-end",
        &format!(
            "Stop it and delete it.\n{}\n{}\n{}",
            call("cancel_task"),
            call("delete_task"),
            call("delete_task")
        ),
    )
    .await;
    assert_eq!(answers[0]["data"]["done"], "cancel", "{answers:#?}");
    assert_eq!(answers[0]["data"]["status"], TaskStatus::Cancelled.as_str());
    assert_eq!(answers[1]["data"]["done"], "deleted", "{answers:#?}");
    assert_eq!(answers[2]["data"]["done"], "already", "{answers:#?}");
    assert!(host.conn().await.task_get(&id).await.is_err());
}

/// In a task's own conversation, Otter stops it for good on the
/// developer's word (D-207), and never from a turn no message started.
#[tokio::test]
async fn otter_stops_its_task_on_the_developers_word() {
    use otter_core::task::{RunState, TaskStatus};
    let fake = chat_host(true).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export HANG").await;
    wait_task(host, &id, "its coding agent at work", |f| {
        f.runs.last().is_some_and(|r| r.state == RunState::Running)
    })
    .await;
    let (_, answers) =
        super::otter_turn::run(host, &id, "t-end-ev", "AS: event\nCALL: cancel_task {}").await;
    assert_eq!(
        code(&answers[0]),
        "not_allowed_in_this_turn",
        "{answers:#?}"
    );
    set_levels(host, &[("cancel_task", "auto")]).await;
    let (_, answers) =
        super::otter_turn::run(host, &id, "t-end-ev2", "AS: event\nCALL: cancel_task {}").await;
    assert_eq!(code(&answers[0]), "needs_developer", "{answers:#?}");
    let (_, answers) = super::otter_turn::run(
        host,
        &id,
        "t-end",
        "Stop it for good.\nCALL: cancel_task {}",
    )
    .await;
    assert_eq!(answers[0]["data"]["done"], "cancel", "{answers:#?}");
    let f = wait_task(host, &id, "cancelled", |f| {
        f.status == TaskStatus::Cancelled
    })
    .await;
    assert_eq!(f.status_reason.as_deref(), Some("Cancelled by you"));
    assert!(f.runs.iter().all(|r| r.state.is_over()));
}
