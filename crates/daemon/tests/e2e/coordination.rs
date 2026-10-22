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

/// A run as `gh run view --json` prints it, for the fake gh.
fn write_run(fake: &FakeCodex, id: &str, status: &str, conclusion: &str) {
    let failed = conclusion == "failure";
    let run = serde_json::json!({
        "name": "CI",
        "displayTitle": "Fix the billing export",
        "status": status,
        "conclusion": conclusion,
        "event": "push",
        "headBranch": "main",
        "headSha": "0123456789abcdef",
        "createdAt": "2026-10-22T10:00:00Z",
        "updatedAt": "2026-10-22T10:05:00Z",
        "url": format!("https://github.com/sugerio/marketplace-service/actions/runs/{id}"),
        "jobs": [
            {"name": "lint", "status": "completed", "conclusion": "success", "steps": []},
            {"name": "test", "status": if status == "completed" { "completed" } else { "in_progress" },
             "conclusion": conclusion,
             "steps": [{"name": "go test ./...", "conclusion": if failed { "failure" } else { "success" }}]},
        ],
    });
    std::fs::write(fake.gh.join(format!("run-{id}.json")), run.to_string()).unwrap();
    std::fs::write(
        fake.gh.join("log"),
        "test\tgo test ./...\t@TIME@ --- FAIL: TestExport (@SECS@)\ntest\tgo test ./...\t@TIME@ export_test.go:42: want 3 rows, got 2\n",
    )
    .unwrap();
}

const RUN_1: &str = "https://github.com/sugerio/marketplace-service/actions/runs/39824362436";
const RUN_2: &str = "https://github.com/sugerio/marketplace-service/actions/runs/39833972735";

/// The answers in a scripted reply, a line each.
fn answers_in(reply: &str) -> Vec<Value> {
    reply
        .lines()
        .filter_map(|l| {
            l.split_once(": ")
                .and_then(|(_, j)| serde_json::from_str(j).ok())
        })
        .collect()
}

fn reply_to_request(f: &Task) -> Option<String> {
    let request = f.request_message()?.to_string();
    f.messages
        .iter()
        .find(|m| {
            m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&request)
        })
        .map(|m| m.text.clone())
}

/// Eyes On CICD (D-192): two run links and no workspace. Otter reads both
/// runs itself, answers, and the task ends — no workspace, no coding agent.
#[tokio::test]
async fn otter_reads_the_runs_itself_and_answers_without_a_workspace() {
    let fake = coord_host("", vec![]).await;
    let host = &fake.host;
    write_run(&fake, "39824362436", "completed", "failure");
    write_run(&fake, "39833972735", "completed", "success");
    let request = format!(
        "{RUN_1}\n{RUN_2}\nCALL: read_github {{\"url\": \"{RUN_1}\"}}\nCALL: read_github {{\"url\": \"{RUN_2}\"}}\nCALL: finish_with_answer {{}}"
    );
    let mut conn = host.conn().await;
    let f = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "eyes".into(),
            title: "Eyes On CICD".into(),
            request,
            workspace: None,
            answer_only: false,
        })
        .await
        .unwrap();
    let id = f.id.to_string();
    conn.task_act("eyes-start", &id, otter_core::task::TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &id, "answered and done", |f| {
        f.status == TaskStatus::Done && reply_to_request(f).is_some()
    })
    .await;
    assert!(f.workspace_id.is_none());
    assert!(f.runs.is_empty());
    let reply = reply_to_request(&f).unwrap();
    let answers = answers_in(&reply);
    assert_eq!(
        answers.iter().map(code).collect::<Vec<_>>(),
        ["ok", "ok", "ok"],
        "{reply}"
    );
    let first = &answers[0]["data"];
    assert_eq!(first["conclusion"], "failure");
    assert_eq!(first["jobs"][1]["failed_steps"][0], "go test ./...");
    let log = first["failed_log_end"].as_str().unwrap();
    assert!(log.contains("want 3 rows, got 2"), "{log}");
    assert!(!log.contains("2026-10-20T"), "times are cut: {log}");
    assert_eq!(answers[1]["data"]["conclusion"], "success");
    assert!(answers[0]["data"]["read_at"].is_string());
    assert!(
        f.status_reason
            .as_deref()
            .unwrap_or_default()
            .starts_with("Otter answered it"),
        "{:?}",
        f.status_reason
    );
    // Nothing was made for it.
    assert!(host.conn().await.workspace_list().await.unwrap().is_empty());
}

/// The same request, its workspace taken by another task's agent (the
/// screenshot's case): Otter reads the runs before the task waits, and the
/// task is done without ever waiting (D-191, D-192).
#[tokio::test]
async fn a_task_whose_workspace_is_taken_is_answered_before_it_waits() {
    let fake = coord_host("set_workspace=developer", vec![]).await;
    let host = &fake.host;
    write_run(&fake, "39824362436", "completed", "failure");
    let dir = host.home().join("marketplace");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Makefile"), "test:\n\t@true\n").unwrap();
    directory(host, "marketplace", &dir).await;
    let busy = task_in(host, "a", "marketplace", "Add CSV export HANG", false).await;
    at_work(host, &busy).await;
    let eyes = task_in(
        host,
        "b",
        "marketplace",
        &format!(
            "{RUN_1}\nCALL: read_github {{\"url\": \"{RUN_1}\"}}\nCALL: finish_with_answer {{}}"
        ),
        false,
    )
    .await;
    let f = wait_task(host, &eyes, "answered and done", |f| {
        f.status == TaskStatus::Done && reply_to_request(f).is_some()
    })
    .await;
    assert!(f.runs.is_empty(), "no coding agent");
    let recs = history(host, &eyes).await;
    assert!(
        !recs
            .iter()
            .any(|r| r.text.contains("Waiting for marketplace")),
        "it never waited: {:#?}",
        recs.iter().map(|r| &r.text).collect::<Vec<_>>()
    );
    let busy = host.conn().await.task_get(&busy).await.unwrap();
    assert!(
        busy.runs
            .last()
            .is_some_and(|r| r.state == RunState::Running)
    );
}

/// A run still going: Otter watches it (kind run, by its link); the task
/// waits on that, not on the developer; when the run ends, the watch's own
/// turn answers and ends the task (D-192).
#[tokio::test]
async fn a_run_still_going_is_watched_and_the_watch_finishes_the_task() {
    let fake = coord_host(
        "",
        vec![
            ("OTTER_WATCH_TICK_MS", "100".into()),
            ("OTTER_WATCH_POLL_MS", "100".into()),
        ],
    )
    .await;
    let host = &fake.host;
    write_run(&fake, "39833972735", "in_progress", "");
    let request = format!(
        "{RUN_2}\nCALL: watch {{\"what\": \"say how it ends CALL: read_github {{\\\"url\\\": \\\"{RUN_2}\\\"}} CALL: finish_with_answer {{}}\", \"kind\": \"run\", \"url\": \"{RUN_2}\"}}"
    );
    let mut conn = host.conn().await;
    let f = conn
        .task_create(otter_protocol::task::TaskCreate {
            command_id: "eyes".into(),
            title: "Eyes On CICD".into(),
            request,
            workspace: None,
            answer_only: false,
        })
        .await
        .unwrap();
    let id = f.id.to_string();
    conn.task_act("eyes-start", &id, otter_core::task::TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &id, "waiting on the watch", |f| {
        f.status_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("Waiting for what Otter watches"))
    })
    .await;
    assert_eq!(
        f.status,
        TaskStatus::Planning,
        "not waiting on the developer"
    );
    assert!(f.needs().is_none());
    let reply = reply_to_request(&f).unwrap();
    let answers = answers_in(&reply);
    assert_eq!(code(&answers[0]), "ok", "{reply}");
    assert_eq!(
        answers[0]["data"]["now"]["text"], "CI: in progress, 1 of 2 jobs not finished",
        "{reply}"
    );
    // The run ends.
    write_run(&fake, "39833972735", "completed", "failure");
    let f = wait_task(
        host,
        &id,
        "the watch's turn finished it, and said so",
        |f| {
            f.status == TaskStatus::Done
                && f.messages.iter().any(|m| {
                    m.correlation_id
                        .as_deref()
                        .is_some_and(|c| c.starts_with("watch:"))
                })
        },
    )
    .await;
    let report = f
        .messages
        .iter()
        .rev()
        .find(|m| {
            m.correlation_id
                .as_deref()
                .is_some_and(|c| c.starts_with("watch:"))
        })
        .map(|m| m.text.clone())
        .unwrap();
    assert!(report.contains("failed: test"), "{report}");
    assert!(report.contains("finish_with_answer: {\"data\""), "{report}");
    assert!(f.workspace_id.is_none() && f.runs.is_empty());
}

/// `read_github` reads a pull request too; a link that isn't GitHub's is
/// refused (D-192).
#[tokio::test]
async fn read_github_reads_a_pull_request_and_refuses_other_links() {
    let fake = coord_host("", vec![]).await;
    let host = &fake.host;
    std::fs::write(fake.gh.join("pr"), "").unwrap();
    std::fs::write(
        fake.gh.join("checks.json"),
        r#"[{"name":"test","bucket":"pass","link":"https://github.com/o/r/actions/runs/1/job/2"}]"#,
    )
    .unwrap();
    let dir = host.home().join("w");
    std::fs::create_dir_all(&dir).unwrap();
    directory(host, "w", &dir).await;
    let id = task_in(host, "a", "w", "Add CSV export HANG", false).await;
    at_work(host, &id).await;
    let (_, answers) = super::otter_turn::run(
        host,
        &id,
        "pr",
        "CALL: read_github {\"url\": \"https://github.com/o/r/pull/7\"}\nCALL: read_github {\"url\": \"https://example.com/x\"}",
    )
    .await;
    assert_eq!(answers[0]["data"]["state"], "OPEN", "{answers:?}");
    assert_eq!(
        answers[0]["data"]["checks"][0]["state"], "pass",
        "{answers:?}"
    );
    assert_eq!(code(&answers[1]), "invalid", "{answers:?}");
}

/// Otter tells the coding agent what to do in its own words (D-192): said
/// in the conversation as Otter's, and to the run as Otter's — never as the
/// developer's — from a turn no message started too.
#[tokio::test]
async fn otter_tells_the_agent_in_its_own_words() {
    let fake = coord_host("", vec![]).await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export HANG").await;
    at_work(host, &id).await;
    let (f, answers) = super::otter_turn::run(
        host,
        &id,
        "tell",
        "AS: event\nCALL: tell_agent {\"text\": \"Look at export_test.go first: it fails on the header.\"}",
    )
    .await;
    assert_eq!(answers[0]["data"]["done"], "told", "{answers:?}");
    let said = f
        .messages
        .iter()
        .find(|m| m.role == MessageRole::Controller && m.text.starts_with("To the coding agent: "))
        .unwrap();
    assert!(said.correlation_id.as_deref().unwrap().starts_with("tell:"));
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
    let turn = conv
        .conversation
        .turns
        .iter()
        .find(|t| t.text().contains("export_test.go"))
        .expect("queued to the run");
    assert!(
        turn.text()
            .starts_with("From Otter, your tech lead (not the developer):"),
        "{}",
        turn.text()
    );
    assert!(!turn.text().contains("From the developer"));
}

/// Publishing and accepting are automatic by default (D-192): the task
/// publishes without asking, Otter accepts it once CI passed, says so, and
/// the ledger has both with what undoes the acceptance — which works.
#[tokio::test]
async fn otter_publishes_and_accepts_on_its_own_and_it_can_be_undone() {
    let fake = coord_host("set_workspace=developer,approve_plan=developer", vec![]).await;
    let host = &fake.host;
    let (id, _) = git_task(&fake).await;
    let f = wait_task(host, &id, "published without asking", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    assert!(
        !f.decisions.iter().any(|d| d.summary.contains("git push")),
        "not asked: {:?}",
        f.decisions
    );
    std::fs::write(
        fake.gh.join("checks.json"),
        r#"[{"name":"test","bucket":"pass","link":"https://github.com/o/r/actions/runs/1/job/2"}]"#,
    )
    .unwrap();
    let f = wait_task(host, &id, "accepted by Otter", |f| {
        f.status == TaskStatus::Done
    })
    .await;
    assert_eq!(
        f.status_reason.as_deref(),
        Some("Accepted by Otter: every gate passed (accepting is automatic in Settings)")
    );
    assert!(
        f.messages
            .iter()
            .any(|m| m.text.contains("I'm accepting it"))
    );
    let actions = host.conn().await.otter_actions(None).await.unwrap();
    let publish = actions
        .iter()
        .find(|a| a.tool == "publish")
        .expect("publish on record");
    assert_eq!(publish.origin, "auto");
    assert_eq!(publish.detail.as_deref(), Some("feature/csv"));
    let accept = actions
        .iter()
        .find(|a| a.tool == "accept_task")
        .expect("acceptance on record");
    assert_eq!(accept.origin, "auto");
    assert_eq!(accept.error, None);
    let undo = accept.undo.clone().expect("it can be undone");
    assert_eq!(undo.task_id.as_str(), id);
    host.conn()
        .await
        .task_act("undo-accept", &id, undo.action)
        .await
        .unwrap();
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert_ne!(f.status, TaskStatus::Done);
}

/// Approving plans is automatic by default (D-192): the plan the review
/// had points on goes back to the agent once, then Otter approves it —
/// the developer isn't asked.
#[tokio::test]
async fn otter_approves_the_plan_after_one_send_back() {
    let fake = coord_host(
        "set_workspace=developer,publish=developer,accept_task=developer",
        vec![("OTTER_PLAN_TURN", "1".into())],
    )
    .await;
    let host = &fake.host;
    let id = started_task(host, "Add CSV export").await;
    let f = wait_task(host, &id, "planned and at work", |f| {
        f.steps.iter().any(|t| !t.planning)
    })
    .await;
    assert!(
        !f.decisions
            .iter()
            .any(|d| d.kind == otter_core::task::DecisionKind::PlanApproval && d.user_only),
        "the developer wasn't asked: {:?}",
        f.decisions
    );
    assert!(
        f.rationale
            .iter()
            .any(|r| r.starts_with("Otter approved the plan")),
        "{:#?}",
        f.rationale
    );
    let actions = host.conn().await.otter_actions(None).await.unwrap();
    assert!(
        actions
            .iter()
            .any(|a| a.tool == "approve_plan" && a.origin == "auto")
    );
}
