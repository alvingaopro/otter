//! A task that makes its own workspace (D-178): from a repository, or an empty
//! directory, in the same command that makes the task; made once whatever
//! repeats; put away with the task. And Otter on the host finding the
//! workspace, or making one, from what the developer said.

use super::host_chat::say;
use super::*;
use otter_core::task::{TaskAction, TaskStatus};
use otter_protocol::task::{NewWorkspace, Scratch, TaskCreate, WorkspaceChoice};

fn create(command_id: &str, title: &str, workspace: WorkspaceChoice) -> TaskCreate {
    TaskCreate {
        command_id: command_id.into(),
        title: title.into(),
        request: "Add a README".into(),
        workspace: Some(workspace),
        answer_only: false,
    }
}

fn from_repo(repo: &Path) -> WorkspaceChoice {
    WorkspaceChoice::New {
        new: NewWorkspace {
            repository: repo.to_string_lossy().into_owned(),
            base: None,
            branch: None,
        },
    }
}

/// The task and its workspace come from one command; a repeat of it neither
/// makes a second task nor a second workspace; the workspace is marked as
/// made for the task and prepared before the work starts.
#[tokio::test]
async fn a_task_makes_its_workspace_from_a_repository_once() {
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let repo = make_repo(host, "billing", &[("Makefile", "test:\n\t@true\n")]);
    let mut conn = host.conn().await;

    let f = conn
        .task_create(create("c-1", "Retry with backoff", from_repo(&repo)))
        .await
        .unwrap();
    let ws_id = f.workspace_id.clone().expect("the task has a workspace");
    let again = conn
        .task_create(create("c-1", "Retry with backoff", from_repo(&repo)))
        .await
        .unwrap();
    assert_eq!(again.id, f.id);
    assert_eq!(again.workspace_id, Some(ws_id.clone()));

    let all = conn.workspace_list().await.unwrap();
    assert_eq!(
        all.len(),
        1,
        "{:?}",
        all.iter().map(|w| &w.name).collect::<Vec<_>>()
    );
    let ws = wait_prepared(host, all[0].name.as_str()).await;
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
    assert_eq!(ws.id, ws_id);
    assert_eq!(ws.task_id.as_ref(), Some(&f.id));
    assert!(
        ws.name.starts_with("billing-retry-with-backoff"),
        "{}",
        ws.name
    );
    assert!(ws.sessions.is_empty());
    let otter_core::WorkspaceSource::Git(g) = &ws.source else {
        panic!("{:?}", ws.source)
    };
    assert!(
        g.requested_branch
            .as_deref()
            .is_some_and(|b| b.starts_with("otter/retry-with-backoff-"))
    );

    // A second task from the same repository gets a workspace of its own.
    let second = conn
        .task_create(create("c-2", "Refund keys", from_repo(&repo)))
        .await
        .unwrap();
    assert_ne!(second.workspace_id, Some(ws_id));
    assert_eq!(conn.workspace_list().await.unwrap().len(), 2);

    // It starts once the workspace is ready.
    conn.task_act("start-1", f.id.as_str(), TaskAction::Start)
        .await
        .unwrap();
    wait_task(host, f.id.as_str(), "it goes to work", |f| {
        !f.runs.is_empty()
    })
    .await;
}

/// A repository that can't be had is said on the task, which waits; it is not
/// started in a half-made workspace.
#[tokio::test]
async fn a_task_from_a_repository_that_is_not_there_waits_and_says_why() {
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let mut conn = host.conn().await;
    let gone = host.home().join("nowhere");
    let f = conn
        .task_create(create("c-1", "Nothing", from_repo(&gone)))
        .await
        .unwrap();
    let name = conn.workspace_list().await.unwrap()[0].name.clone();
    let ws = wait_prepared(host, &name).await;
    assert_eq!(ws.state, WorkspaceState::Failed);
    conn.task_act("start-1", f.id.as_str(), TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, f.id.as_str(), "it waits, saying why", |f| {
        f.status_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("Waiting for ") && r.contains("preparing it failed"))
    })
    .await;
    assert!(f.runs.is_empty());
}

/// An empty directory for a task; and an empty repository name is refused
/// before anything is made.
#[tokio::test]
async fn a_task_can_have_an_empty_directory_and_refuses_an_empty_repository() {
    let fake = fake_agent_host("ok", "rules").await;
    let host = &fake.host;
    let mut conn = host.conn().await;
    let f = conn
        .task_create(create(
            "c-1",
            "CSV to JSON",
            WorkspaceChoice::Scratch {
                scratch: Scratch {},
            },
        ))
        .await
        .unwrap();
    let ws = conn
        .workspace_get(f.workspace_id.unwrap().as_str())
        .await
        .unwrap();
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert!(matches!(ws.source, otter_core::WorkspaceSource::Empty));
    assert!(ws.name.starts_with("scratch-csv-to-json"), "{}", ws.name);
    assert_eq!(ws.task_id.as_ref(), Some(&f.id));

    let err = conn
        .task_create(create(
            "c-2",
            "Nothing",
            WorkspaceChoice::New {
                new: NewWorkspace {
                    repository: "  ".into(),
                    base: None,
                    branch: None,
                },
            },
        ))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("repository"), "{err:#}");
    assert_eq!(conn.workspace_list().await.unwrap().len(), 1);
}

/// Otter on the host: finds the workspace that comes from the repository the
/// developer named, sees what is working in it, lists the repositories here,
/// and makes a task in a workspace of its own from a repository, on the
/// developer's word.
#[tokio::test]
async fn otter_finds_a_workspace_and_makes_one_for_a_task() {
    let fake =
        fake_agent_host_with("ok", "scripted", |_| vec![("OTTER_BRAIN_V2", "1".into())]).await;
    let host = &fake.host;
    let repo = make_repo(host, "billing", &[("Makefile", "test:\n\t@true\n")]);
    let mut conn = host.conn().await;
    conn.workspace_create(git_ws("billing", &repo, None, vec![]))
        .await
        .unwrap();
    wait_prepared(host, "billing").await;
    let other = make_repo(host, "web", &[("a", "b")]);
    conn.workspace_create(git_ws("web", &other, None, vec![]))
        .await
        .unwrap();
    wait_prepared(host, "web").await;

    let text = format!(
        "Add retries in billing.\nCALL: list_workspaces {{\"query\": \"billing\"}}\nCALL: list_repositories {{}}\nCALL: create_task {{\"title\": \"Retries\", \"message_ids\": $MSG, \"new_workspace\": {{\"repository\": \"{}\"}}, \"start\": true}}",
        repo.display()
    );
    let (_, _, answers) = say(host, "h-1", &text).await;
    let found = answers[0]["data"]["workspaces"].as_array().unwrap();
    assert_eq!(found.len(), 1, "{answers:?}");
    assert_eq!(found[0]["name"], "billing");
    assert!(found[0]["task_working_in_it"].is_null());
    assert_eq!(
        answers[1]["data"]["repositories"].as_array().unwrap().len(),
        2,
        "{answers:?}"
    );
    assert_eq!(answers[2]["data"]["done"], "created", "{answers:?}");
    let made = answers[2]["data"]["task_id"].as_str().unwrap().to_owned();
    let f = host.conn().await.task_get(&made).await.unwrap();
    let ws = host
        .conn()
        .await
        .workspace_get(f.workspace_id.unwrap().as_str())
        .await
        .unwrap();
    assert_ne!(ws.name, "billing");
    assert_eq!(ws.task_id.as_ref().map(|t| t.as_str()), Some(made.as_str()));
    assert_ne!(f.status, TaskStatus::Draft, "started");

    // Both at once is refused, and nothing is made.
    let text = "Both.\nCALL: create_task {\"title\": \"x\", \"message_ids\": $MSG, \"workspace\": \"web\", \"scratch\": true}";
    let (_, _, answers) = say(host, "h-2", text).await;
    assert_eq!(answers[0]["error"]["code"], "invalid", "{answers:?}");
}
