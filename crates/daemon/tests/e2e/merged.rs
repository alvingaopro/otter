//! A task's own workspace goes once its work is merged (D-189). The fake
//! `gh` says what GitHub would of the pull request; otterd looks at the
//! tasks' archived workspaces every few hundred milliseconds.

use super::*;
use otter_core::task::{MessageRole, Task, TaskAction, TaskStatus};
use otter_protocol::task::{NewWorkspace, TaskCreate, WorkspaceChoice};

async fn host() -> FakeCodex {
    fake_agent_host_with("ok", "rules", |_| {
        vec![("OTTER_MERGED_SWEEP_MS", "200".into())]
    })
    .await
}

/// A task in a workspace made for it (D-179) on `feature/csv`, published
/// and accepted: done, its workspace archived (D-105). The task and its
/// workspace.
async fn finished(fake: &FakeCodex) -> (String, Workspace) {
    let src = fake.host.home().join("origin");
    std::fs::create_dir_all(src.join(".github/workflows")).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("Makefile"), "test:\n\t@echo all good\n").unwrap();
    std::fs::write(src.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "init"]);
    let mut conn = fake.host.conn().await;
    let f = conn
        .task_create(TaskCreate {
            command_id: "create".into(),
            title: "CSV export".into(),
            request: "Export the timeline as CSV WRITE:timeline.csv".into(),
            workspace: Some(WorkspaceChoice::New {
                new: NewWorkspace {
                    repository: src.display().to_string(),
                    base: None,
                    branch: Some("feature/csv".into()),
                },
            }),
            answer_only: false,
        })
        .await
        .unwrap();
    let id = f.id.to_string();
    conn.task_act("start", &id, TaskAction::Start)
        .await
        .unwrap();
    allow_publish(&fake.host, &id).await;
    let f = wait_task(&fake.host, &id, "published", |f| {
        f.delivery.as_ref().is_some_and(|d| d.pr_url.is_some())
    })
    .await;
    let ws = f.workspace_id.clone().unwrap();
    conn.task_act(
        "accept",
        &id,
        TaskAction::Accept {
            override_gates: true,
        },
    )
    .await
    .unwrap();
    let ws = eventually("its workspace archived", || async {
        let ws = fake
            .host
            .conn()
            .await
            .workspace_get(ws.as_str())
            .await
            .unwrap();
        (ws.state == WorkspaceState::Archived).then_some(ws)
    })
    .await;
    assert_eq!(ws.task_id.as_ref().map(|t| t.as_str()), Some(id.as_str()));
    (id, ws)
}

/// What the fake GitHub says of pull request #7.
fn github(fake: &FakeCodex, state: &str, merged_at: Option<&str>, head: &str) {
    std::fs::write(fake.gh.join("pr-state"), state).unwrap();
    std::fs::write(fake.gh.join("pr-head"), head).unwrap();
    match merged_at {
        Some(t) => std::fs::write(fake.gh.join("merged-at"), t).unwrap(),
        None => {
            let _ = std::fs::remove_file(fake.gh.join("merged-at"));
        }
    }
}

/// Otter's lines in the task's conversation that contain `text`.
fn said(f: &Task, text: &str) -> usize {
    f.messages
        .iter()
        .filter(|m| m.role == MessageRole::Controller && m.text.contains(text))
        .count()
}

/// Several passes of the sweep.
async fn passes() {
    tokio::time::sleep(Duration::from_millis(1200)).await;
}

async fn sweep_setting(host: &TestHost, on: bool) {
    let s = host
        .conn()
        .await
        .settings_set(otter_protocol::host::SettingsUpdate {
            delete_merged_workspaces: Some(on),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(s.delete_merged_workspaces, Some(on));
}

#[tokio::test]
async fn a_tasks_workspace_goes_once_its_pull_request_is_merged() {
    let fake = host().await;
    let host = &fake.host;
    let (id, ws) = finished(&fake).await;
    let head = git(Path::new(&ws.root), &["rev-parse", "HEAD"]);
    let head = head.trim();
    let gone = |ws: &Workspace| {
        let id = ws.id.to_string();
        async move { host.conn().await.workspace_get(&id).await.is_err() }
    };

    // Open, merged within the day, or the setting off: it stays.
    github(&fake, "OPEN", None, head);
    passes().await;
    assert!(!gone(&ws).await, "open");
    let calls = std::fs::read_to_string(fake.gh.join("calls.log")).unwrap();
    assert!(
        calls.contains(
            "gh pr view 7 --json state,mergeable,headRefOid,baseRefName,mergedAt --repo o/r"
        ),
        "{calls}"
    );
    let now = chrono::Utc::now().to_rfc3339();
    github(&fake, "MERGED", Some(&now), head);
    passes().await;
    assert!(!gone(&ws).await, "merged an instant ago");
    sweep_setting(host, false).await;
    github(&fake, "MERGED", Some("2026-01-01T00:00:00Z"), head);
    passes().await;
    assert!(!gone(&ws).await, "the setting is off");

    // On: merged a day ago or more, it goes, worktree and branch with it
    // (D-108), and the conversation says so.
    sweep_setting(host, true).await;
    eventually("deleted", || async { gone(&ws).await.then_some(()) }).await;
    assert!(!Path::new(&ws.root).exists());
    let f = wait_task(host, &id, "it says so", |f| {
        said(f, "is deleted: pull request #7 was merged") > 0
    })
    .await;
    assert_eq!(
        said(
            &f,
            "everything in it is on main. Asked for more, the task starts again from main"
        ),
        1
    );
    assert_eq!(f.status, TaskStatus::Done);
    assert_eq!(f.workspace_id, None);
    let g = f.workspace_gone.clone().unwrap();
    assert_eq!((g.name.as_str(), g.count), (ws.name.as_str(), 1));
    assert_eq!(g.base.as_deref(), Some("main"));
    assert!(g.repository.unwrap().ends_with("origin"));

    // Asked for more: a new workspace of its own, on a new branch from
    // main, and the work goes on there.
    let runs = f.runs.len();
    host.conn()
        .await
        .task_act(
            "more",
            &id,
            TaskAction::RequestChanges {
                note: Some("And a LICENSE".into()),
            },
        )
        .await
        .unwrap();
    let f = wait_task(host, &id, "a new workspace", |f| f.workspace_id.is_some()).await;
    let again = host
        .conn()
        .await
        .workspace_get(f.workspace_id.as_ref().unwrap().as_str())
        .await
        .unwrap();
    assert_ne!(again.id, ws.id);
    assert_eq!(
        again.task_id.as_ref().map(|t| t.as_str()),
        Some(id.as_str())
    );
    let otter_core::WorkspaceSource::Git(g) = &again.source else {
        panic!("{:?}", again.source)
    };
    assert_eq!(g.base.as_deref(), Some("origin/main"));
    assert_eq!(said(&f, "the work goes on in a new one"), 1);
    let f = wait_task(host, &id, "working again", |f| f.runs.len() > runs).await;
    let again = host
        .conn()
        .await
        .workspace_get(again.id.as_str())
        .await
        .unwrap();
    let otter_core::WorkspaceSource::Git(g) = &again.source else {
        panic!("{:?}", again.source)
    };
    assert!(
        g.branch.starts_with("otter/csv-export-") && g.branch.ends_with("-2"),
        "{}",
        g.branch
    );
    assert_ne!(f.status, TaskStatus::Failed, "{:?}", f.status_reason);
}

#[tokio::test]
async fn a_merged_tasks_workspace_stays_while_something_is_only_there() {
    let fake = host().await;
    let host = &fake.host;
    let (id, ws) = finished(&fake).await;
    let root = Path::new(&ws.root);
    let head = git(root, &["rev-parse", "HEAD"]);
    let head = head.trim().to_owned();

    // A file Git doesn't track: kept, said once, and not tried again.
    std::fs::write(root.join("notes.txt"), "mine\n").unwrap();
    github(&fake, "MERGED", Some("2026-01-01T00:00:00Z"), &head);
    let why = "stays although pull request #7 is merged: it holds files Git doesn't track";
    wait_task(host, &id, "kept", |f| said(f, why) > 0).await;
    let kept = host
        .conn()
        .await
        .workspace_get(ws.id.as_str())
        .await
        .unwrap();
    assert_eq!(kept.state, WorkspaceState::Archived);
    assert_eq!(
        kept.kept.as_deref(),
        Some("it holds files Git doesn't track")
    );
    passes().await;
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert_eq!(said(&f, why), 1);
    assert_eq!(f.workspace_id.as_ref(), Some(&ws.id));

    // Unarchived, it is the developer's again; archived again, it is
    // looked at again.
    let back = host
        .conn()
        .await
        .workspace_unarchive(ws.id.as_str())
        .await
        .unwrap();
    assert_eq!(back.kept, None);
    eventually("ready", || async {
        let w = host
            .conn()
            .await
            .workspace_get(ws.id.as_str())
            .await
            .unwrap();
        (w.state == WorkspaceState::Ready).then_some(())
    })
    .await;
    std::fs::remove_file(root.join("notes.txt")).unwrap();
    // A commit pushed after the pull request: on the remote, but not in
    // what was merged.
    std::fs::write(root.join("Makefile"), "test:\n\t@echo still good\n").unwrap();
    git(root, &["commit", "-qam", "After the merge"]);
    git(root, &["push", "-q", "origin", "feature/csv"]);
    host.conn()
        .await
        .workspace_archive(ws.id.as_str())
        .await
        .unwrap();
    let why = "its branch has commits that aren't in the pull request";
    wait_task(host, &id, "kept again", |f| said(f, why) > 0).await;
    let kept = host
        .conn()
        .await
        .workspace_get(ws.id.as_str())
        .await
        .unwrap();
    assert_eq!(kept.kept.as_deref(), Some(why));
}
