//! Otter's conversation on a host (D-093, plan M3): about any task there,
//! with the host's tools — looking across tasks, making, starting, pausing
//! them — answered once per message, and kept across a restart.

use super::*;
use otter_core::feature::{FeatureEvent, FeatureStatus, MessageRole, OtterChat};
use serde_json::Value;

async fn chat_host(v2: bool) -> FakeCodex {
    fake_agent_host_with("ok", "scripted", move |_| {
        vec![("OTTER_BRAIN_V2", if v2 { "1" } else { "0" }.into())]
    })
    .await
}

/// Say `text` to Otter on the host; wait for its one reply. The chat then,
/// the reply, and each tool call's answer in order.
async fn say(host: &TestHost, command_id: &str, text: &str) -> (OtterChat, String, Vec<Value>) {
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
    let first = started_feature(host, "Add CSV export HANG").await;
    wait_feature(host, &first, "at work", |f| {
        f.status == FeatureStatus::Implementing
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
    let f = host.conn().await.feature_get(&made).await.unwrap();
    // The request is the developer's words, as written; the title Otter's.
    assert_eq!(f.request, text);
    assert_eq!(f.title, "Fix the login form");
    assert!(f.workspace_id.is_some());
    assert_ne!(f.status, FeatureStatus::Draft, "started");
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
        FeatureEvent::ToolCalled { tool, authority, .. }
            if tool == "create_task" && authority.as_deref() == Some(msg.as_str())
    )));
    // Kept across a restart.
    let before = chat.messages.len();
    fake.host.restart().await;
    let chat = fake.host.conn().await.otter_get().await.unwrap();
    assert_eq!(chat.messages.len(), before);
}

/// A turn an event started can pause a task, never make or start one.
#[tokio::test]
async fn otter_on_the_host_acts_on_the_developers_word_only() {
    let fake = chat_host(true).await;
    let host = &fake.host;
    let first = started_feature(host, "Add CSV export HANG").await;
    wait_feature(host, &first, "at work", |f| {
        f.status == FeatureStatus::Implementing
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
    wait_feature(host, &first, "paused", |f| {
        f.status == FeatureStatus::Paused
    })
    .await;
    assert_eq!(host.conn().await.feature_list().await.unwrap().len(), 1);
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
    let tasks = host.conn().await.feature_list().await.unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:#?}");
    let f = &tasks[0];
    println!("task: {} — {:?}", f.title, f.status);
    assert_eq!(f.request, text, "the developer's words");
    assert_eq!(f.status, FeatureStatus::Draft, "not started");
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
    use otter_core::feature::FeatureAction;
    let fake = chat_host(true).await;
    let host = &fake.host;
    project(host, "app").await;
    let other = started_feature(host, "Add a footer").await;
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
    let chat = eventually("a follow-up on review", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        (!follow_ups(&c).is_empty()).then_some(c)
    })
    .await;
    let first = follow_ups(&chat);
    assert_eq!(first.len(), 1, "{first:#?}");
    assert!(
        first[0].starts_with(&format!("Follow-up: [{made}] CSV export is now review")),
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
    // The next change has its own.
    wait_feature(host, &made, "gates checked", |f| !f.gates.is_empty()).await;
    host.conn()
        .await
        .feature_act(
            "accept-made",
            &made,
            FeatureAction::Accept {
                override_gates: false,
            },
        )
        .await
        .unwrap();
    let chat = eventually("a follow-up on done", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        (follow_ups(&c).len() == 2).then_some(c)
    })
    .await;
    assert!(
        follow_ups(&chat)[1].contains("is now done"),
        "{:#?}",
        follow_ups(&chat)
    );
    // The task it didn't make reached review too, unremarked.
    wait_feature(host, &other, "in review", |f| {
        f.status == FeatureStatus::Review
    })
    .await;
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
            host.conn().await.feature_list().await.unwrap()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    for f in follow_ups(&chat) {
        println!("follow-up: {f}");
    }
    let tasks = host.conn().await.feature_list().await.unwrap();
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
