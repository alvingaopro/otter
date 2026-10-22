//! Otter coordinates the work itself (D-190–D-193): where a task works can
//! change, Otter reads what it needs, acts on its own where that is
//! automatic, and makes sub-tasks.

use super::*;
use otter_core::task::{MessageRole, RunState, Task, TaskEvent, TaskStatus};
use serde_json::Value;

/// A host with Otter's tools (the scripted brain); `levels` replaces the
/// harness's (D-190).
async fn coord_host(levels: &str, more: Vec<(&'static str, String)>) -> FakeCodex {
    let levels = levels.to_owned();
    fake_agent_host_with("ok", "scripted", move |_| {
        more.into_iter()
            .chain([
                ("OTTER_BRAIN_V2", "1".into()),
                ("OTTER_TOOL_LEVELS", levels),
            ])
            .collect()
    })
    .await
}

/// The same, its coding agent the fake SDK worker in `mode`.
async fn coord_sdk_host(levels: &str, mode: &str) -> FakeCodex {
    let (levels, mode) = (levels.to_owned(), mode.to_owned());
    fake_agent_host_with("ok", "scripted", move |scratch| {
        let worker = scratch.join("worker.sh");
        std::fs::write(&worker, include_str!("../fake_claude_worker.sh")).unwrap();
        vec![
            ("OTTER_BRAIN_V2", "1".into()),
            ("OTTER_TOOL_LEVELS", levels),
            ("OTTER_CODING_BACKEND", "sdk".into()),
            ("OTTER_NODE", "/bin/sh".into()),
            ("OTTER_CLAUDE_WORKER", worker.to_string_lossy().into_owned()),
            ("FAKE_WORKER_MODE", mode),
        ]
    })
    .await
}

/// A directory workspace `name` at `dir`.
async fn directory(host: &TestHost, name: &str, dir: &Path) {
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

/// A task in workspace `ws`, started.
async fn task_in(host: &TestHost, cmd: &str, ws: &str, request: &str, answer_only: bool) -> String {
    let mut conn = host.conn().await;
    let f = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: cmd.into(),
            title: request
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" "),
            request: request.into(),
            workspace: Some(ws.into()),
            answer_only,
        })
        .await
        .unwrap();
    conn.task_act(
        &format!("{cmd}-start"),
        f.id.as_str(),
        otter_core::task::TaskAction::Start,
    )
    .await
    .unwrap();
    f.id.to_string()
}

async fn at_work(host: &TestHost, id: &str) -> Task {
    wait_task(host, id, "the agent at work", |f| {
        f.status == TaskStatus::Implementing
            && f.runs.last().is_some_and(|r| r.state == RunState::Running)
    })
    .await
}

async fn history(host: &TestHost, id: &str) -> Vec<otter_core::task::TaskEventRecord> {
    host.conn().await.task_events(id, None).await.unwrap()
}

fn code(answer: &Value) -> &str {
    answer["error"]["code"].as_str().unwrap_or("ok")
}

/// A directory the developer keeps, a clone of a repository: the task
/// whose agent can't start there because another works there gets a
/// workspace of its own from that repository, on the branch the directory
/// has (D-191). The directory itself is left as it was.
#[tokio::test]
async fn a_task_whose_directory_is_taken_gets_a_workspace_of_its_own() {
    let fake = coord_host(
        "publish=developer,approve_plan=developer,accept_task=developer",
        vec![],
    )
    .await;
    let host = &fake.host;
    let origin = make_repo(
        host,
        "origin-src",
        &[("Makefile", "test:\n\t@echo all good\n")],
    );
    let dev = host.home().join("dev");
    git(
        host.home(),
        &[
            "clone",
            "-q",
            &origin.to_string_lossy(),
            &dev.to_string_lossy(),
        ],
    );
    directory(host, "dev", &dev).await;
    let first = task_in(host, "a", "dev", "Add CSV export HANG", false).await;
    at_work(host, &first).await;

    let second = task_in(host, "b", "dev", "Add retries HANG", false).await;
    let f = wait_task(host, &second, "moved to a workspace of its own", |f| {
        f.runs.last().is_some_and(|r| r.state == RunState::Running)
    })
    .await;
    let dev_ws = host.conn().await.workspace_get("dev").await.unwrap();
    let own = host
        .conn()
        .await
        .workspace_get(f.workspace_id.as_ref().unwrap().as_str())
        .await
        .unwrap();
    assert_ne!(own.id, dev_ws.id);
    assert_eq!(
        own.task_id.as_ref().map(|t| t.as_str()),
        Some(second.as_str())
    );
    match &own.source {
        WorkspaceSource::Git(g) => {
            assert_eq!(g.repository, origin.to_string_lossy());
            assert_eq!(g.base.as_deref(), Some("origin/main"));
        }
        other => panic!("not from the repository: {other:?}"),
    }
    let said = f
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Controller && m.text.contains("a workspace of its own"))
        .map(|m| m.text.clone())
        .unwrap_or_default();
    assert!(said.starts_with("dev is taken"), "{said}");
    // Otter read the request before it was moved, once.
    assert!(
        f.messages.iter().any(|m| m.role == MessageRole::Controller
            && m.correlation_id.as_deref() == f.request_message().map(|r| r.as_str())),
        "the request was read"
    );
    let recs = history(host, &second).await;
    assert!(recs.iter().any(|r| matches!(&r.event,
        TaskEvent::ToolCalled { tool, origin, detail: Some(d), .. }
            if tool == "set_workspace" && origin == "auto" && d.starts_with("dev → "))));
    // Both at work, each in its own directory; the developer's is as it was.
    let first = host.conn().await.task_get(&first).await.unwrap();
    assert_eq!(first.workspace_id.as_ref(), Some(&dev_ws.id));
    assert!(
        first
            .runs
            .last()
            .is_some_and(|r| r.state == RunState::Running)
    );
    let trees = std::process::Command::new("git")
        .args(["-C", &dev.to_string_lossy(), "worktree", "list"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&trees.stdout).lines().count(), 1);
}

/// Where moving is the developer's word (D-190), the task waits, as before.
#[tokio::test]
async fn where_moving_is_the_developers_word_the_task_waits() {
    let fake = coord_host("set_workspace=developer", vec![]).await;
    let host = &fake.host;
    let origin = make_repo(host, "origin-src", &[("Makefile", "test:\n\t@true\n")]);
    let dev = host.home().join("dev");
    git(
        host.home(),
        &[
            "clone",
            "-q",
            &origin.to_string_lossy(),
            &dev.to_string_lossy(),
        ],
    );
    directory(host, "dev", &dev).await;
    let first = task_in(host, "a", "dev", "Add CSV export HANG", false).await;
    at_work(host, &first).await;
    let second = task_in(host, "b", "dev", "Add retries", false).await;
    let f = wait_task(host, &second, "waiting", |f| {
        f.status_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("Waiting for dev"))
    })
    .await;
    let dev_ws = host.conn().await.workspace_get("dev").await.unwrap();
    assert_eq!(f.workspace_id.as_ref(), Some(&dev_ws.id));
    assert!(f.runs.is_empty());
}

/// An answer-only task reads alongside the work in its workspace (D-191):
/// its run starts though another task's runs there, and the other's run
/// isn't held up either.
#[tokio::test]
async fn an_answer_only_task_reads_alongside_the_work() {
    let fake = coord_sdk_host(
        "publish=developer,approve_plan=developer,accept_task=developer",
        "hang",
    )
    .await;
    let host = &fake.host;
    let dir = host.home().join("shared");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@true\n").unwrap();
    directory(host, "shared", &dir).await;
    let writer = task_in(host, "w", "shared", "Add CSV export", false).await;
    at_work(host, &writer).await;
    let reader = task_in(host, "r", "shared", "How does the export work?", true).await;
    let f = at_work(host, &reader).await;
    let ws = host.conn().await.workspace_get("shared").await.unwrap();
    assert_eq!(f.workspace_id.as_ref(), Some(&ws.id));
    let w = host.conn().await.task_get(&writer).await.unwrap();
    assert!(w.runs.last().is_some_and(|r| r.state == RunState::Running));
    // The listing says the writer holds it, never the reader.
    let (_, answers) =
        super::otter_turn::run(host, &reader, "look", "CALL: list_workspaces {}").await;
    let held = answers[0]["data"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "shared")
        .unwrap()["held_by_task"]
        .clone();
    assert_eq!(held, Value::String(writer.clone()), "{answers:?}");
    let _ = code;
}
