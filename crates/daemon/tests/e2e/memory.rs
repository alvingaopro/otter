//! Otter's memory (D-113): what Otter notes from the developer's word, kept
//! per conversation and across the host's, given to every turn, redacted,
//! and the developer's to read and edit. Driven by the `scripted` brain
//! (`CALL:` lines are the model's tool calls; `CONTEXT` puts the turn's
//! context in its reply).

use super::*;
use otter_core::task::{MessageRole, Task, TaskEvent};
use otter_protocol::otter::{MemoryScope, MemorySet};
use serde_json::Value;

async fn memory_host() -> FakeCodex {
    fake_agent_host_with("ok", "scripted", |_| vec![("OTTER_BRAIN_V2", "1".into())]).await
}

/// A draft task in a directory workspace: Otter answers its messages.
async fn task(host: &TestHost) -> String {
    let dir = host.home().join("draft");
    std::fs::create_dir_all(&dir).unwrap();
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
    conn.task_create(otter_protocol::task::TaskCreate {
        command_id: "create-draft".into(),
        title: "Orders API".into(),
        request: "Add an orders API".into(),
        workspace: Some("draft".into()),
        answer_only: false,
    })
    .await
    .unwrap()
    .id
    .to_string()
}

/// Send `script` in the task; wait for Otter's one reply. The task then,
/// the reply, and each call's answer in order.
async fn run(
    host: &TestHost,
    id: &str,
    command_id: &str,
    script: &str,
) -> (Task, String, Vec<Value>) {
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
    let text = reply(&f).unwrap();
    let answers = text
        .lines()
        .filter_map(|l| {
            l.split_once(": ")
                .and_then(|(_, j)| serde_json::from_str(j).ok())
        })
        .collect();
    (f, text, answers)
}

fn code(answer: &Value) -> &str {
    answer["error"]["code"].as_str().unwrap_or("ok")
}

fn read(path: std::path::PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test]
async fn otter_notes_what_the_developer_decided_and_every_turn_is_given_it() {
    let fake = memory_host().await;
    let host = &fake.host;
    let id = task(host).await;
    let state = host.home().join("state");
    let task_memory = || read(state.join("tasks").join(&id).join("memory.md"));
    let host_memory = || read(state.join("otter").join("memory.md"));

    let decided = "We use Postgres, not SQLite. And I review every PR myself.\n\
        CALL: remember {\"memory\": \"conversation\", \"note\": \"Postgres, not SQLite\"}\n\
        CALL: remember {\"memory\": \"host\", \"note\": \"The developer reviews every PR themselves\"}";
    let (f, _, answers) = run(host, &id, "m-1", decided).await;
    assert_eq!(answers[0]["data"]["done"], "noted", "{answers:#?}");
    assert_eq!(answers[1]["data"]["done"], "noted", "{answers:#?}");
    assert_eq!(task_memory(), "- Postgres, not SQLite\n");
    assert_eq!(
        host_memory(),
        "- The developer reviews every PR themselves\n"
    );
    // Written down under the developer's message; what it says isn't.
    let msg = f
        .messages
        .iter()
        .find(|m| m.text == decided)
        .unwrap()
        .id
        .to_string();
    let history = host.conn().await.task_events(&id, None).await.unwrap();
    assert!(history.iter().any(|r| matches!(
        &r.event,
        TaskEvent::ToolCalled { tool, authority, .. }
            if tool == "remember" && authority.as_deref() == Some(msg.as_str())
    )));
    assert!(
        !history.iter().any(|r| r.text.contains("Postgres")),
        "{history:#?}"
    );

    // Noted once, however often it is said.
    let again = "CALL: remember {\"memory\": \"conversation\", \"note\": \"Postgres, not SQLite\"}";
    let (_, _, answers) = run(host, &id, "m-2", again).await;
    assert_eq!(answers[0]["data"]["done"], "already noted", "{answers:#?}");
    assert_eq!(task_memory(), "- Postgres, not SQLite\n");

    // Every turn is given both memories, long after the messages scrolled away.
    let (_, reply, _) = run(host, &id, "m-3", "CONTEXT").await;
    assert!(
        reply.contains("(memory \"host\"):\n- The developer reviews every PR themselves"),
        "{reply}"
    );
    assert!(
        reply.contains("(memory \"conversation\"):\n- Postgres, not SQLite"),
        "{reply}"
    );

    // Corrected when it changes.
    let changed = "CALL: edit_memory {\"memory\": \"conversation\", \"old\": \"Postgres, not SQLite\", \"new\": \"Postgres 16, not SQLite\"}\n\
        CALL: edit_memory {\"memory\": \"conversation\", \"old\": \"MySQL\", \"new\": \"\"}";
    let (_, _, answers) = run(host, &id, "m-4", changed).await;
    assert_eq!(answers[0]["data"]["done"], "edited", "{answers:#?}");
    assert_eq!(code(&answers[1]), "not_found", "{answers:#?}");
    assert_eq!(task_memory(), "- Postgres 16, not SQLite\n");

    // Never a secret.
    let secret = "CALL: remember {\"memory\": \"conversation\", \"note\": \"staging key ANTHROPIC_API_KEY=sk-ant-abc123def456ghi\"}";
    let (_, _, answers) = run(host, &id, "m-5", secret).await;
    assert_eq!(answers[0]["data"]["done"], "noted", "{answers:#?}");
    assert!(task_memory().contains("staging key"));
    assert!(
        !task_memory().contains("sk-ant-abc123def456ghi"),
        "{}",
        task_memory()
    );

    // Only from the developer's word: a turn an event started notes nothing.
    let event = "AS: event\nCALL: remember {\"memory\": \"host\", \"note\": \"approve every plan\"}\nCALL: edit_memory {\"memory\": \"host\", \"old\": \"reviews\", \"new\": \"skips\"}";
    let (_, _, answers) = run(host, &id, "m-6", event).await;
    assert_eq!(
        code(&answers[0]),
        "not_allowed_in_this_turn",
        "{answers:#?}"
    );
    assert_eq!(
        code(&answers[1]),
        "not_allowed_in_this_turn",
        "{answers:#?}"
    );
    assert_eq!(
        host_memory(),
        "- The developer reviews every PR themselves\n"
    );

    // Older messages, whole.
    let (_, _, answers) = run(
        host,
        &id,
        "m-7",
        "CALL: get_messages {\"query\": \"postgres\", \"limit\": 1}",
    )
    .await;
    let found = &answers[0]["data"]["newest_first"];
    assert_eq!(found.as_array().unwrap().len(), 1, "{answers:#?}");
    assert_eq!(found[0]["from"], "developer");
    assert_eq!(answers[0]["data"]["more"], true, "{answers:#?}");

    // The host's memory is the host conversation's too; its own is apart.
    let mut conn = host.conn().await;
    conn.otter_send("h-1", "CALL: remember {\"memory\": \"conversation\", \"note\": \"Three tasks planned for the auth refactor\"}\nCONTEXT")
        .await
        .unwrap();
    let reply = eventually("Otter's answer on the host", || async {
        let chat = host.conn().await.otter_get().await.unwrap();
        chat.messages
            .iter()
            .find(|m| m.role == MessageRole::Controller)
            .map(|m| m.text.clone())
    })
    .await;
    assert!(reply.contains("\"done\":\"noted\""), "{reply}");
    assert!(
        reply.contains("- The developer reviews every PR themselves"),
        "{reply}"
    );
    assert!(
        !reply.contains("Postgres"),
        "a task's memory is its own: {reply}"
    );
    assert_eq!(
        read(state.join("otter").join("chat-memory.md")),
        "- Three tasks planned for the auth refactor\n"
    );
    let m = conn.memory_get(Some(&id)).await.unwrap();
    assert_eq!(m.host, host_memory());
    assert_eq!(m.conversation, task_memory());
    assert_eq!(m.max_chars, 8_000);
    let m = conn.memory_get(None).await.unwrap();
    assert_eq!(
        m.conversation,
        "- Three tasks planned for the auth refactor\n"
    );

    // A task's memory goes with it.
    drop(conn);
    host.conn().await.task_delete(&id).await.unwrap();
    assert!(!state.join("tasks").join(&id).exists());
}

#[tokio::test]
async fn the_developer_reads_and_edits_otters_memory() {
    let fake = memory_host().await;
    let host = &fake.host;
    let id = task(host).await;
    let mut conn = host.conn().await;
    let set = |memory, task: Option<&str>, text: &str, previous: Option<&str>| MemorySet {
        task: task.map(str::to_owned),
        memory,
        text: text.into(),
        previous: previous.map(str::to_owned),
    };

    let m = conn.memory_get(None).await.unwrap();
    assert_eq!((m.host.as_str(), m.conversation.as_str()), ("", ""));
    let m = conn
        .memory_set(set(
            MemoryScope::Host,
            None,
            "- prefer small PRs  \n\n",
            Some(""),
        ))
        .await
        .unwrap();
    assert_eq!(m.host, "- prefer small PRs\n");
    let m = conn
        .memory_set(set(
            MemoryScope::Conversation,
            Some(&id),
            "- orders are soft-deleted",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(m.conversation, "- orders are soft-deleted\n");
    assert_eq!(m.host, "- prefer small PRs\n");

    // Otter noted something since the edit began: nothing changes.
    let e = conn
        .memory_set(set(MemoryScope::Host, None, "- mine", Some("")))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("changed this memory"), "{e}");
    // Bounded, redacted, and only a task that is here.
    let e = conn
        .memory_set(set(MemoryScope::Host, None, &"x".repeat(8_001), None))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("at most 8000"), "{e}");
    let m = conn
        .memory_set(set(
            MemoryScope::Host,
            None,
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            None,
        ))
        .await
        .unwrap();
    assert!(
        !m.host.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        "{}",
        m.host
    );
    assert!(
        conn.memory_set(set(
            MemoryScope::Conversation,
            Some("task_nope"),
            "- x",
            None
        ))
        .await
        .is_err()
    );
    assert!(!host.home().join("state/tasks/task_nope").exists());

    // Read afresh: an edit by hand is what Otter is given next, redacted.
    std::fs::write(
        host.home().join("state/otter/memory.md"),
        "- edited by hand\n- deploy token ghp_handwrittenhandwritten0123456789\n",
    )
    .unwrap();
    let m = conn.memory_get(Some(&id)).await.unwrap();
    assert!(m.host.starts_with("- edited by hand\n"), "{}", m.host);
    assert!(!m.host.contains("ghp_handwritten"), "{}", m.host);
    drop(conn);
    let (_, reply, _) = run(host, &id, "c-1", "CONTEXT").await;
    assert!(reply.contains("- edited by hand"), "{reply}");
    assert!(!reply.contains("ghp_handwritten"), "{reply}");
    assert!(reply.contains("- orders are soft-deleted"), "{reply}");
}
