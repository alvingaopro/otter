//! A task that makes its own workspace (D-179): from a repository, or an empty
//! directory, in the same command that makes the task; made once whatever
//! repeats; put away with the task. And Otter on the host finding the
//! workspace, or making one, from what the developer said.

use super::host_chat::say;
use super::*;
use otter_core::task::{Task, TaskAction, TaskStatus};
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
    assert!(found[0]["held_by_task"].is_null());
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

/// What Otter reads in a listing can be handed back: a repository shown
/// without its credentials names the stored one, so the task's workspace is
/// of the same backing repository (and authenticates), not a second one.
#[tokio::test]
async fn a_repository_as_listed_names_the_stored_one() {
    let fake =
        fake_agent_host_with("ok", "scripted", |_| vec![("OTTER_BRAIN_V2", "1".into())]).await;
    let host = &fake.host;
    let mut conn = host.conn().await;
    let url = "https://user:s3cr3t-token@127.0.0.1:9/acme/billing.git";
    conn.workspace_create(WorkspaceCreate {
        name: "billing".into(),
        source: SourceSpec::Git {
            repository: url.into(),
            branch: None,
            base: None,
        },
        sessions: Some(vec![]),
        ..Default::default()
    })
    .await
    .unwrap();
    let (_, reply, answers) = say(
        host,
        "h-1",
        "What is here?\nCALL: list_repositories {}\nCALL: list_workspaces {\"query\": \"acme\"}",
    )
    .await;
    assert!(!reply.contains("s3cr3t"), "{reply}");
    let listed = answers[0]["data"]["repositories"][0]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!listed.contains("s3cr3t"), "{listed}");
    assert_eq!(
        answers[1]["data"]["workspaces"].as_array().unwrap().len(),
        1,
        "{answers:?}"
    );

    let f = conn
        .task_create(create(
            "c-1",
            "Retries",
            WorkspaceChoice::New {
                new: NewWorkspace {
                    repository: listed,
                    base: None,
                    branch: None,
                },
            },
        ))
        .await
        .unwrap();
    let all = conn.workspace_list().await.unwrap();
    let git = |w: &Workspace| match &w.source {
        otter_core::WorkspaceSource::Git(g) => g.repo_id.clone(),
        other => panic!("{other:?}"),
    };
    let made = all
        .iter()
        .find(|w| w.id == f.workspace_id.clone().unwrap())
        .unwrap();
    let old = all.iter().find(|w| w.name == "billing").unwrap();
    assert_eq!(git(made), git(old));
}

/// A branch or base that Git would take for an option is refused.
#[tokio::test]
async fn a_branch_that_looks_like_an_option_is_refused() {
    let fake = fake_agent_host("ok", "rules").await;
    let mut conn = fake.host.conn().await;
    for (base, branch) in [(Some("--upload-pack=x"), None), (None, Some("a b"))] {
        let err = conn
            .task_create(create(
                "c-1",
                "x",
                WorkspaceChoice::New {
                    new: NewWorkspace {
                        repository: "/r".into(),
                        base: base.map(Into::into),
                        branch: branch.map(Into::into),
                    },
                },
            ))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("one word"), "{err:#}");
    }
    assert!(conn.task_list().await.unwrap().is_empty());
    assert!(conn.workspace_list().await.unwrap().is_empty());
}

/// Otter is shown which task holds a workspace — a run there, or work of
/// another task there that isn't done — so it makes the next task a
/// workspace of its own instead of one that would wait; and a refused
/// request makes nothing.
#[tokio::test]
async fn otter_sees_what_holds_a_workspace_and_a_refused_request_makes_nothing() {
    let fake =
        fake_agent_host_with("ok", "scripted", |_| vec![("OTTER_BRAIN_V2", "1".into())]).await;
    let host = &fake.host;
    // In a Git workspace, so the hold between runs (D-156) counts too.
    let first = started_with(
        host,
        "proj",
        "Add CSV export HANG",
        "test:\n\t@echo ok\n",
        true,
    )
    .await;
    wait_task(host, &first, "at work", |f| f.live_run().is_some()).await;
    let (_, _, answers) = say(
        host,
        "h-1",
        "Who has proj?\nCALL: list_workspaces {\"query\": \"proj\"}",
    )
    .await;
    let found = answers[0]["data"]["workspaces"].as_array().unwrap();
    assert_eq!(found.len(), 1, "{answers:?}");
    assert_eq!(found[0]["held_by_task"], first.as_str(), "{answers:?}");
    // Paused, it holds the workspace still: its work is there, not done.
    host.conn()
        .await
        .task_act("pause-1", &first, TaskAction::Pause)
        .await
        .unwrap();
    wait_task(host, &first, "paused", |f| f.status == TaskStatus::Paused).await;
    let (_, _, answers) = say(
        host,
        "h-1b",
        "And now?\nCALL: list_workspaces {\"query\": \"proj\"}",
    )
    .await;
    assert_eq!(
        answers[0]["data"]["workspaces"][0]["held_by_task"],
        first.as_str(),
        "{answers:?}"
    );

    let tasks = host.conn().await.task_list().await.unwrap().len();
    let workspaces = host.conn().await.workspace_list().await.unwrap().len();
    let text = "Both.\nCALL: create_task {\"title\": \"x\", \"message_ids\": $MSG, \"workspace\": \"proj\", \"scratch\": true}";
    let (_, _, answers) = say(host, "h-2", text).await;
    assert_eq!(answers[0]["error"]["code"], "invalid", "{answers:?}");
    assert_eq!(host.conn().await.task_list().await.unwrap().len(), tasks);
    assert_eq!(
        host.conn().await.workspace_list().await.unwrap().len(),
        workspaces
    );
}

// --- Otter in a task's conversation places the task (D-186) ---

use super::otter_turn::{code, run};
use otter_core::task::MessageRole;
use serde_json::Value;

/// A host whose Otter is scripted, with a Git workspace `billing`.
async fn host_with_billing() -> (FakeCodex, std::path::PathBuf) {
    let fake =
        fake_agent_host_with("ok", "scripted", |_| vec![("OTTER_BRAIN_V2", "1".into())]).await;
    let repo = make_repo(
        &fake.host,
        "billing-src",
        &[("Makefile", "test:\n\t@true\n")],
    );
    let mut conn = fake.host.conn().await;
    conn.workspace_create(git_ws("billing", &repo, None, vec![]))
        .await
        .unwrap();
    wait_prepared(&fake.host, "billing").await;
    (fake, repo)
}

/// A task with no workspace, in the developer's words.
async fn unplaced(host: &TestHost, command_id: &str, request: &str) -> Task {
    host.conn()
        .await
        .task_create(TaskCreate {
            command_id: command_id.into(),
            title: "Retries".into(),
            request: request.into(),
            workspace: None,
            answer_only: false,
        })
        .await
        .unwrap()
}

/// Otter's reply to the task's request, if it gave one.
fn reply_to_request(f: &Task) -> Option<String> {
    let request = f.request_message()?.to_string();
    f.messages
        .iter()
        .find(|m| {
            m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&request)
        })
        .map(|m| m.text.clone())
}

/// The point of D-186: the developer writes where, once, in the request; the
/// task is started with no workspace; Otter finds it from the request and the
/// work begins — nobody chooses a workspace.
#[tokio::test]
async fn a_started_task_is_placed_from_its_request() {
    let (fake, _) = host_with_billing().await;
    let host = &fake.host;
    let f = unplaced(
        host,
        "c-1",
        "Add retries in billing.\nCALL: list_workspaces {\"query\": \"billing\"}\nCALL: set_workspace {\"workspace\": \"billing\"}",
    )
    .await;
    assert!(f.workspace_id.is_none());
    let id = f.id.to_string();
    host.conn()
        .await
        .task_act("start-1", &id, TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &id, "it goes to work in billing", |f| {
        !f.runs.is_empty()
    })
    .await;
    let ws = host
        .conn()
        .await
        .workspace_get(f.workspace_id.clone().unwrap().as_str())
        .await
        .unwrap();
    assert_eq!(ws.name, "billing");
    let reply = reply_to_request(&f).expect("Otter answered the request");
    assert!(reply.contains("set_workspace"), "{reply}");
}

/// …and from a repository the request names: a workspace of its own is
/// made, marked as made for the task, and the work waits for it.
#[tokio::test]
async fn a_started_task_gets_a_workspace_made_from_the_repository_it_names() {
    let (fake, repo) = host_with_billing().await;
    let host = &fake.host;
    let other = make_repo(host, "ledger-src", &[("Makefile", "test:\n\t@true\n")]);
    let _ = repo;
    let request = format!(
        "Add retries to the ledger.\nCALL: set_workspace {{\"new_workspace\": {{\"repository\": \"{}\"}}}}",
        other.display()
    );
    let f = unplaced(host, "c-1", &request).await;
    let id = f.id.to_string();
    host.conn()
        .await
        .task_act("start-1", &id, TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &id, "it goes to work", |f| !f.runs.is_empty()).await;
    let ws = host
        .conn()
        .await
        .workspace_get(f.workspace_id.unwrap().as_str())
        .await
        .unwrap();
    assert_ne!(ws.name, "billing");
    assert_eq!(ws.task_id.as_ref().map(|t| t.as_str()), Some(id.as_str()));
    assert_eq!(ws.state, WorkspaceState::Ready, "{:?}", ws.state_message);
}

/// Not clear where: the task waits, Otter has said so, and the developer's
/// answer in the conversation places it, and it goes on by itself. Only the
/// developer's word does it, and not once the agent has worked.
#[tokio::test]
async fn a_task_without_a_clear_place_asks_and_goes_on_when_answered() {
    let (fake, _) = host_with_billing().await;
    let host = &fake.host;
    let f = unplaced(host, "c-1", "Add retries.").await;
    let id = f.id.to_string();
    host.conn()
        .await
        .task_act("start-1", &id, TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &id, "it waits for a workspace", |f| {
        f.status == TaskStatus::Blocked && reply_to_request(f).is_some()
    })
    .await;
    assert!(f.workspace_id.is_none());

    // An event's turn can't place it.
    let (f, answers) = run(
        host,
        &id,
        "ev-1",
        "AS: event\nCALL: set_workspace {\"scratch\": true}",
    )
    .await;
    assert_eq!(code(&answers[0]), "not_allowed_in_this_turn", "{answers:?}");
    assert!(f.workspace_id.is_none());
    assert_eq!(host.conn().await.workspace_list().await.unwrap().len(), 1);

    // Both ways at once is refused; nothing is made.
    let (_, answers) = run(
        host,
        &id,
        "both-1",
        "Both.\nCALL: set_workspace {\"workspace\": \"billing\", \"scratch\": true}",
    )
    .await;
    assert_eq!(code(&answers[0]), "invalid", "{answers:?}");
    assert_eq!(host.conn().await.workspace_list().await.unwrap().len(), 1);

    // The developer answers.
    let (_, answers) = run(
        host,
        &id,
        "dev-1",
        "In billing, please.\nCALL: set_workspace {\"workspace\": \"billing\"}",
    )
    .await;
    assert_eq!(code(&answers[0]), "ok", "{answers:?}");
    let f = wait_task(host, &id, "it goes to work", |f| !f.runs.is_empty()).await;
    assert!(f.workspace_id.is_some());

    // Worked in it: not moved.
    let (_, answers) = run(
        host,
        &id,
        "dev-2",
        "Somewhere else.\nCALL: set_workspace {\"scratch\": true}",
    )
    .await;
    assert_eq!(code(&answers[0]), "not_ready", "{answers:?}");
}

/// Otter looks at the request once: a task paused and started again doesn't
/// have it asked again.
#[tokio::test]
async fn otter_reads_the_request_for_a_workspace_once() {
    let (fake, _) = host_with_billing().await;
    let host = &fake.host;
    let f = unplaced(host, "c-1", "Add retries.").await;
    let id = f.id.to_string();
    let mut conn = host.conn().await;
    conn.task_act("start-1", &id, TaskAction::Start)
        .await
        .unwrap();
    wait_task(host, &id, "it waits", |f| {
        f.status == TaskStatus::Blocked && reply_to_request(f).is_some()
    })
    .await;
    conn.task_act("pause-1", &id, TaskAction::Pause)
        .await
        .unwrap();
    conn.task_act("resume-1", &id, TaskAction::Resume)
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let f = conn.task_get(&id).await.unwrap();
    let said = f
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Controller)
        .count();
    assert_eq!(
        said,
        1,
        "{:?}",
        f.messages.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}

/// While Otter looks at the request, the developer adds something: it is
/// answered, not taken for answered by the reply to the request.
#[tokio::test]
async fn a_message_sent_while_otter_reads_the_request_is_answered() {
    let (fake, _) = host_with_billing().await;
    let host = &fake.host;
    let f = unplaced(host, "c-1", "Add retries.\nSLEEP: 1500").await;
    let id = f.id.to_string();
    let mut conn = host.conn().await;
    conn.task_act("start-1", &id, TaskAction::Start)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let f = conn
        .task_send("m-1", &id, "And use the staging branch.")
        .await
        .unwrap();
    let said = f
        .messages
        .iter()
        .find(|m| m.role == MessageRole::User && m.text.starts_with("And use"))
        .unwrap()
        .id
        .to_string();
    wait_task(host, &id, "it is answered", |f| {
        f.messages.iter().any(|m| {
            m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(&said)
        })
    })
    .await;
}

/// The turn on a request may look and say where the task works; it may not
/// pause the task, change what it may do, or pass the request on.
#[tokio::test]
async fn the_turn_on_a_request_may_only_look_and_place() {
    let (fake, _) = host_with_billing().await;
    let host = &fake.host;
    let f = unplaced(
        host,
        "c-1",
        "Add retries in billing.\nCALL: pause_task {}\nCALL: set_answer_only {\"on\": true}\nCALL: forward_to_agent {\"message_ids\": $MSG}\nCALL: set_workspace {\"workspace\": \"billing\"}",
    )
    .await;
    let id = f.id.to_string();
    host.conn()
        .await
        .task_act("start-1", &id, TaskAction::Start)
        .await
        .unwrap();
    let f = wait_task(host, &id, "it goes to work", |f| !f.runs.is_empty()).await;
    assert!(!f.answer_only);
    let reply = reply_to_request(&f).unwrap();
    let answers: Vec<Value> = reply
        .lines()
        .filter_map(|l| {
            l.split_once(": ")
                .and_then(|(_, j)| serde_json::from_str(j).ok())
        })
        .collect();
    let codes: Vec<&str> = answers.iter().map(code).collect();
    assert_eq!(
        codes,
        [
            "not_allowed_in_this_turn",
            "not_allowed_in_this_turn",
            "not_allowed_in_this_turn",
            "ok"
        ],
        "{reply}"
    );
}

/// A task with a workspace made from one repository isn't told it is placed
/// when asked for another source; the same again is done, once.
#[tokio::test]
async fn set_workspace_says_when_the_workspace_it_has_is_not_what_was_asked() {
    let (fake, _) = host_with_billing().await;
    let host = &fake.host;
    let ledger = make_repo(host, "ledger-src", &[("Makefile", "test:\n\t@true\n")]);
    let f = unplaced(host, "c-1", "Add retries.").await;
    let id = f.id.to_string();
    let new = format!(
        "In the ledger.\nCALL: set_workspace {{\"new_workspace\": {{\"repository\": \"{}\"}}}}",
        ledger.display()
    );
    let (_, answers) = run(host, &id, "d-1", &new).await;
    assert_eq!(code(&answers[0]), "ok", "{answers:?}");
    let made = host.conn().await.workspace_list().await.unwrap().len();

    let (_, answers) = run(
        host,
        &id,
        "d-2",
        "Empty.\nCALL: set_workspace {\"scratch\": true}",
    )
    .await;
    assert_eq!(code(&answers[0]), "conflict", "{answers:?}");
    let (_, answers) = run(
        host,
        &id,
        "d-3",
        "Billing.\nCALL: set_workspace {\"workspace\": \"billing\"}",
    )
    .await;
    assert_eq!(code(&answers[0]), "conflict", "{answers:?}");
    // The same again: done, and nothing more is made.
    let new = new.replace("In the ledger.", "Again, the ledger.");
    let (_, answers) = run(host, &id, "d-4", &new).await;
    assert_eq!(code(&answers[0]), "ok", "{answers:?}");
    assert_eq!(
        host.conn().await.workspace_list().await.unwrap().len(),
        made
    );
}
