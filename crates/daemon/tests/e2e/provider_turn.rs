//! Otter's turn through a model provider's API (D-095, plan M5): the tool
//! loop runs in otterd, against a stand-in for the API (OpenAI-style and
//! Anthropic-style), with the same gateway as Claude's turn.

use super::*;
use otter_core::task::{MessageRole, TaskEvent};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A stand-in for a provider's API on 127.0.0.1. A tool turn looks the task
/// up (`get_task`); when the prompt says FORWARD, it then passes the
/// developer's last message on by the id the prompt gives
/// (`forward_to_agent`); then it answers with what it was told. Every
/// request is kept: its head and body.
struct FakeApi {
    base: String,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The tool results a request carries, in order (either API's shape).
fn results(messages: &[Value]) -> Vec<String> {
    let mut out = vec![];
    for m in messages {
        if m["role"] == "tool" {
            out.push(m["content"].as_str().unwrap_or_default().to_owned());
        }
        if let Some(blocks) = m["content"].as_array() {
            for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                out.push(b["content"].as_str().unwrap_or_default().to_owned());
            }
        }
    }
    out
}

fn answer(head: &str, body: &str) -> Value {
    let anthropic = head.contains("/messages");
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let messages = v["messages"].as_array().cloned().unwrap_or_default();
    let prompt = messages
        .iter()
        .find_map(|m| {
            (m["role"] == "user")
                .then(|| m["content"].as_str())
                .flatten()
        })
        .unwrap_or_default()
        .to_owned();
    let got = results(&messages);
    let field = |text: &str, name: &str| {
        let key = format!("\"{name}\":\"");
        text.find(&key)
            .map(|i| {
                text[i + key.len()..]
                    .split('"')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_default()
    };
    let call = |name: &str, input: Value| {
        if anthropic {
            json!({"content": [{"type": "tool_use", "id": format!("tu_{}", got.len()), "name": name, "input": input}], "stop_reason": "tool_use"})
        } else {
            json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": format!("call_{}", got.len()), "type": "function", "function": {"name": name, "arguments": input.to_string()}}
            ]}}]})
        }
    };
    let say = |text: String| {
        if anthropic {
            json!({"content": [{"type": "text", "text": text}], "stop_reason": "end_turn"})
        } else {
            json!({"choices": [{"message": {"role": "assistant", "content": text}}]})
        }
    };
    let forward = prompt.contains("FORWARD");
    match got.len() {
        0 => call("get_task", json!({})),
        1 if forward => {
            let id = prompt
                .rsplit('[')
                .find_map(|p| p.split(']').next().filter(|s| s.starts_with("msg_")))
                .unwrap_or_default()
                .to_owned();
            call("forward_to_agent", json!({"message_ids": [id]}))
        }
        _ => {
            let status = field(&got[0], "status");
            match got.get(1) {
                Some(f) => say(format!(
                    "The task is {status}; your message: {}.",
                    field(f, "done")
                )),
                None => say(format!("The task is {status}.")),
            }
        }
    }
}

async fn fake_api() -> FakeApi {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let keep = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let keep = keep.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head, at) = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = find(&buf, b"\r\n\r\n") {
                        break (String::from_utf8_lossy(&buf[..i]).to_string(), i + 4);
                    }
                };
                let len = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if head.to_ascii_lowercase().contains("expect: 100-continue") {
                    let _ = sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
                }
                while buf.len() < at + len {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = String::from_utf8_lossy(&buf[at..(at + len).min(buf.len())]).to_string();
                keep.lock().unwrap().push((head.clone(), body.clone()));
                let reply = answer(&head, &body).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    FakeApi {
        base: format!("http://{addr}/v1"),
        seen,
    }
}

/// A draft task in a workspace of its own: nothing plans it.
async fn draft(host: &TestHost) -> String {
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
        title: "CSV export".into(),
        request: "Add CSV export".into(),
        workspace: Some("draft".into()),
        answer_only: false,
    })
    .await
    .unwrap()
    .id
    .to_string()
}

/// One provider's turn, end to end: Otter looks with its tools, passes the
/// developer's message on by id, and answers; the key goes only in a header.
async fn provider_answers_with_otters_tools(controller: &'static str, key_var: &'static str) {
    let api = fake_api().await;
    let base = api.base.clone();
    let fake = fake_agent_host_with("ok", controller, move |_| {
        vec![
            ("OTTER_BRAIN_V2", "1".into()),
            (key_var, "sk-test-OTTERKEY123".into()),
            ("OTTER_PROVIDER_BASE", base),
            ("OTTER_CONTROLLER_MODEL", "some-model".into()),
        ]
    })
    .await;
    let host = &fake.host;
    assert_eq!(
        host.conn().await.host_status().await.unwrap().otter,
        Some(format!("with tools ({controller})"))
    );
    let id = draft(host).await;
    let text = "Where is it? FORWARD";
    let f = host
        .conn()
        .await
        .task_send("api-1", &id, text)
        .await
        .unwrap();
    let msg = f
        .messages
        .iter()
        .rev()
        .find(|m| m.text == text)
        .unwrap()
        .id
        .clone();
    let f = wait_task(host, &id, "Otter's answer", |f| {
        f.messages
            .iter()
            .any(|m| m.correlation_id.as_deref() == Some(msg.as_str()))
    })
    .await;
    let reply = f
        .messages
        .iter()
        .find(|m| {
            m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(msg.as_str())
        })
        .unwrap();
    assert_eq!(reply.text, "The task is draft; your message: passed on.");
    let recs = host.conn().await.task_events(&id, None).await.unwrap();
    let forwarded = recs.iter().find_map(|r| match &r.event {
        TaskEvent::ToolCalled {
            tool, authority, ..
        } if tool == "forward_to_agent" => Some(authority.clone()),
        _ => None,
    });
    assert_eq!(forwarded, Some(Some(msg.to_string())));
    // Three rounds: look, pass on, answer. The key in a header, never in
    // what is sent; Otter's tools offered.
    let seen = api.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "{seen:#?}");
    for (head, body) in &seen {
        assert!(head.contains("sk-test-OTTERKEY123"), "{head}");
        assert!(!body.contains("sk-test-OTTERKEY123"));
        assert!(body.contains("forward_to_agent") && body.contains("get_task"));
    }
}

#[tokio::test]
async fn an_openai_style_provider_answers_with_otters_tools() {
    provider_answers_with_otters_tools("openrouter", "OPENROUTER_API_KEY").await;
}

#[tokio::test]
async fn an_anthropic_style_provider_answers_with_otters_tools() {
    provider_answers_with_otters_tools("anthropic", "ANTHROPIC_API_KEY").await;
}

/// A real provider's API (D-095): OpenRouter with the key in this machine's
/// environment and a small model, asked about a draft task's history — a
/// tool-using turn, no worker. Run by hand: `cargo test -p otterd --test e2e
/// live_an_api_provider -- --ignored --nocapture` (OPENROUTER_API_KEY set).
#[tokio::test]
#[ignore = "calls OpenRouter with the key in the environment"]
async fn live_an_api_provider_answers_with_otters_tools() {
    std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY");
    let host = TestHost::with_env(&[
        ("OTTER_CONTROLLER", "openrouter".into()),
        ("OTTER_CONTROLLER_MODEL", "openai/gpt-4o-mini".into()),
        ("OTTER_BRAIN_V2", "1".into()),
        ("OTTER_CONTROLLER_TICK_MS", "200".into()),
    ])
    .await;
    assert_eq!(
        host.conn()
            .await
            .host_status()
            .await
            .unwrap()
            .otter
            .as_deref(),
        Some("with tools (openrouter)")
    );
    let id = draft(&host).await;
    let text = "Look at this task's history and tell me where it stands.";
    let f = host
        .conn()
        .await
        .task_send("live-api", &id, text)
        .await
        .unwrap();
    let msg = f
        .messages
        .iter()
        .rev()
        .find(|m| m.text == text)
        .unwrap()
        .id
        .clone();
    let f = wait_task(&host, &id, "Otter's answer", |f| {
        f.messages
            .iter()
            .any(|m| m.correlation_id.as_deref() == Some(msg.as_str()))
    })
    .await;
    let reply = f
        .messages
        .iter()
        .find(|m| {
            m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(msg.as_str())
        })
        .unwrap();
    println!("reply: {}", reply.text);
    let looked: Vec<String> = host
        .conn()
        .await
        .task_events(&id, None)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|r| match r.event {
            TaskEvent::ToolDone { tool, .. } => Some(tool),
            _ => None,
        })
        .collect();
    println!("tools: {looked:?}");
    assert!(!looked.is_empty(), "it looked with its tools");
    assert!(
        reply.text.to_lowercase().contains("draft"),
        "{}",
        reply.text
    );
    assert!(!reply.text.contains("I couldn't look"), "{}", reply.text);
}
