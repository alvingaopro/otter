//! Otter in Slack (D-200): a direct message to the Slack app is said in
//! Otter's conversation on the host and answered in Slack — against a
//! stand-in Slack: its Web API over HTTP, its Socket Mode over a WebSocket.

use super::*;
use futures_util::{SinkExt, StreamExt};
use otter_core::task::{MessageRole, OtterChat};
use otter_protocol::host::{SettingsUpdate, SlackUpdate};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as Frame;

/// A Web API call the stand-in got: the method, the `Authorization`
/// header, the body.
type Call = (String, String, Value);

struct FakeSlack {
    /// Its Web API, for `OTTER_SLACK_API`.
    api: String,
    calls: Arc<Mutex<Vec<Call>>>,
    sockets: tokio::sync::mpsc::UnboundedReceiver<WebSocketStream<TcpStream>>,
}

impl FakeSlack {
    async fn start() -> FakeSlack {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/api", http.local_addr().unwrap());
        let link = format!("ws://{}/link", ws.local_addr().unwrap());
        let calls: Arc<Mutex<Vec<Call>>> = Default::default();
        let kept = calls.clone();
        let posted: Arc<std::sync::atomic::AtomicU64> = Default::default();
        tokio::spawn(async move {
            while let Ok((s, _)) = http.accept().await {
                tokio::spawn(answer(s, link.clone(), kept.clone(), posted.clone()));
            }
        });
        let (tx, sockets) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((s, _)) = ws.accept().await {
                if let Ok(ws) = tokio_tungstenite::accept_async(s).await {
                    let _ = tx.send(ws);
                }
            }
        });
        FakeSlack {
            api,
            calls,
            sockets,
        }
    }

    fn calls(&self, method: &str) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == method)
            .cloned()
            .collect()
    }

    /// The next connection otterd opens, said hello to.
    async fn connected(&mut self) -> WebSocketStream<TcpStream> {
        let mut ws = tokio::time::timeout(Duration::from_secs(15), self.sockets.recv())
            .await
            .expect("otterd connects to Slack")
            .unwrap();
        ws.send(Frame::text(r#"{"type":"hello","num_connections":1}"#))
            .await
            .unwrap();
        ws
    }
}

/// One Web API call: kept, and answered as Slack would.
async fn answer(
    mut s: TcpStream,
    link: String,
    calls: Arc<Mutex<Vec<Call>>>,
    posted: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let header = |name: &str| {
        head.lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
            })
            .unwrap_or_default()
    };
    if header("expect").eq_ignore_ascii_case("100-continue") {
        s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.unwrap();
    }
    let length: usize = header("content-length").parse().unwrap_or(0);
    while buf.len() < head_end + length {
        let n = s.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let path = head
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    // A file Slack keeps (D-202): a picture too large, its thumbnail, a PDF.
    if let Some(name) = path.strip_prefix("/files/") {
        let bytes: Vec<u8> = match name {
            "big.png" => [&b"\x89PNG\r\n\x1a\n"[..], &vec![0u8; 800 * 1024]].concat(),
            "small.png" => b"\x89PNG\r\n\x1a\nsmall".to_vec(),
            _ => b"%PDF-1.7".to_vec(),
        };
        calls
            .lock()
            .unwrap()
            .push(("file".into(), header("authorization"), json!(name)));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.write_all(&bytes).await;
        return;
    }
    let body: Value = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
    let method = head
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or_default()
        .to_owned();
    let auth = header("authorization");
    let reply = match method.as_str() {
        "auth.test" if auth.contains("xoxb-bad") => json!({"ok": false, "error": "invalid_auth"}),
        "auth.test" => json!({"ok": true, "team": "Test Team", "user": "otter", "user_id": "UBOT"}),
        "apps.connections.open" => json!({"ok": true, "url": link}),
        "chat.postMessage" => {
            // Each message its own time, as Slack's are.
            let n = posted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            json!({"ok": true, "ts": format!("1800000000.{n:06}")})
        }
        "reactions.add" | "chat.update" => json!({"ok": true}),
        "conversations.open" => json!({"ok": true, "channel": {"id": "D1"}}),
        _ => json!({"ok": false, "error": "unknown_method"}),
    };
    calls.lock().unwrap().push((method, auth, body));
    let reply = reply.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = s.write_all(response.as_bytes()).await;
}

/// A direct message as Socket Mode sends it.
fn dm(envelope: &str, user: &str, ts: &str, text: &str, extra: Value) -> String {
    let mut event = json!({
        "type": "message", "channel_type": "im", "channel": "D1",
        "user": user, "ts": ts, "text": text,
    });
    for (k, v) in extra.as_object().unwrap() {
        event[k] = v.clone();
    }
    json!({"type": "events_api", "envelope_id": envelope, "payload": {"event": event}}).to_string()
}

/// Send `frame` and read otterd's ack of `envelope`.
async fn acked(ws: &mut WebSocketStream<TcpStream>, frame: String, envelope: &str) {
    ws.send(Frame::text(frame)).await.unwrap();
    let ack = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("an ack")
        .unwrap()
        .unwrap();
    let ack: Value = serde_json::from_str(ack.to_text().unwrap()).unwrap();
    assert_eq!(ack["envelope_id"], envelope);
}

fn said(chat: &OtterChat, text: &str) -> usize {
    chat.messages
        .iter()
        .filter(|m| m.role == MessageRole::User && m.text == text)
        .count()
}

async fn slack_host(slack: &FakeSlack) -> FakeCodex {
    let api = slack.api.clone();
    fake_agent_host_with("ok", "scripted", move |_| {
        vec![("OTTER_BRAIN_V2", "1".into()), ("OTTER_SLACK_API", api)]
    })
    .await
}

fn set_up(bot: &str) -> SettingsUpdate {
    SettingsUpdate {
        secrets: [
            ("SLACK_BOT_TOKEN".to_owned(), Some(bot.to_owned())),
            (
                "SLACK_APP_TOKEN".to_owned(),
                Some("xapp-1-SLACKTESTTOKEN".to_owned()),
            ),
        ]
        .into(),
        slack: Some(SlackUpdate {
            users: Some(vec!["U01DEV".into()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The developer writes to the app in Slack: Otter hears it once, marks it
/// seen, and answers there — in the thread it was written in, if any.
/// Someone else's message and a bot's aren't taken; what the developer then
/// says in the terminal is answered there, not in Slack. The tokens are in
/// no file but the secrets'.
#[tokio::test]
async fn a_direct_message_in_slack_is_answered_there() {
    let mut slack = FakeSlack::start().await;
    let fake = slack_host(&slack).await;
    let host = &fake.host;
    let view = host
        .conn()
        .await
        .settings_set(set_up("xoxb-1-SLACKTESTTOKEN"))
        .await
        .unwrap();
    let s = view.slack.unwrap();
    assert_ne!(s.state, "off", "{s:?}");
    assert_eq!(s.users, vec!["U01DEV"]);
    let mut ws = slack.connected().await;
    eventually("connected", || async {
        let s = host.conn().await.settings_get().await.unwrap().slack?;
        (s.state == "connected").then_some(s)
    })
    .await;
    let s = host
        .conn()
        .await
        .settings_get()
        .await
        .unwrap()
        .slack
        .unwrap();
    assert_eq!(
        (s.team.as_deref(), s.bot.as_deref()),
        (Some("Test Team"), Some("otter"))
    );

    // The developer's message: in Otter's conversation once, seen, answered.
    let question = "What is going on? <https://github.com/acme/api|github.com/acme/api>";
    acked(
        &mut ws,
        dm("e1", "U01DEV", "1700000000.000100", question, json!({})),
        "e1",
    )
    .await;
    let chat = eventually("the message in Otter's conversation", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        (said(&c, "What is going on? https://github.com/acme/api") == 1).then_some(c)
    })
    .await;
    let m = chat
        .messages
        .iter()
        .find(|m| m.role == MessageRole::User)
        .unwrap();
    assert_eq!(
        m.correlation_id.as_deref(),
        Some("slack:D1:1700000000.000100")
    );
    let posted = eventually("Otter's answer in Slack", || async {
        let p = slack.calls("chat.postMessage");
        (!p.is_empty()).then_some(p)
    })
    .await;
    let reply = host
        .conn()
        .await
        .otter_get()
        .await
        .unwrap()
        .messages
        .into_iter()
        .find(|r| r.role == MessageRole::Controller)
        .unwrap();
    let (_, auth, body) = &posted[0];
    assert_eq!(auth, "Bearer xoxb-1-SLACKTESTTOKEN");
    assert_eq!(body["channel"], "D1");
    assert_eq!(body["blocks"][0]["type"], "markdown");
    assert!(body.get("thread_ts").is_none(), "{body}");
    // Shown as it is written, then the answer in its place: one message
    // (D-202).
    assert_eq!(
        body["blocks"][0]["text"],
        format!("{} …", reply.text).as_str()
    );
    let update = eventually("the answer in its place", || async {
        slack
            .calls("chat.update")
            .into_iter()
            .find(|c| c.2["ts"] == "1800000000.000001")
    })
    .await;
    assert_eq!(update.2["blocks"][0]["text"], reply.text.as_str());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(slack.calls("chat.postMessage").len(), 1);
    let seen = slack.calls("reactions.add");
    assert_eq!(seen[0].2["timestamp"], "1700000000.000100");
    assert_eq!(seen[0].2["name"], "eyes");

    // Slack sends it again (its ack was late): nothing more is said.
    acked(
        &mut ws,
        dm("e1b", "U01DEV", "1700000000.000100", question, json!({})),
        "e1b",
    )
    .await;
    // Not the developer, and a bot: acked, not taken.
    acked(
        &mut ws,
        dm(
            "e2",
            "U02OTHER",
            "1700000001.000100",
            "hi from U2",
            json!({}),
        ),
        "e2",
    )
    .await;
    acked(
        &mut ws,
        dm(
            "e3",
            "U01DEV",
            "1700000002.000100",
            "a bot's line",
            json!({"bot_id": "B1"}),
        ),
        "e3",
    )
    .await;

    // Written in a thread: answered in it.
    acked(
        &mut ws,
        dm(
            "e4",
            "U01DEV",
            "1700000003.000100",
            "and in the thread?",
            json!({"thread_ts": "1699.5"}),
        ),
        "e4",
    )
    .await;
    let posted = eventually("the answer in the thread", || async {
        let p = slack.calls("chat.postMessage");
        (p.len() == 2).then_some(p)
    })
    .await;
    assert_eq!(posted[1].2["thread_ts"], "1699.5");
    let chat = host.conn().await.otter_get().await.unwrap();
    assert_eq!(
        said(&chat, "What is going on? https://github.com/acme/api"),
        1
    );
    assert_eq!(said(&chat, "hi from U2") + said(&chat, "a bot's line"), 0);

    // From the terminal: answered there only.
    let (_, _, _) = host_chat::say(host, "cli-1", "and from the terminal").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(slack.calls("chat.postMessage").len(), 2);

    // Nowhere in what otterd writes but its secrets file.
    fn scan(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                scan(&p, found);
            } else if p.file_name().is_some_and(|n| n != "secrets.json")
                && std::fs::read(&p).is_ok_and(|b| b.windows(14).any(|w| w == b"SLACKTESTTOKEN"))
            {
                found.push(p);
            }
        }
    }
    let mut found = Vec::new();
    scan(host.home(), &mut found);
    assert!(found.is_empty(), "{found:?}");
}

/// Settings say what Slack still needs, refuse the tokens swapped, say why
/// a connection failed, and — the token fixed — that it is connecting, not
/// the old failure, until it is connected.
#[tokio::test]
async fn slack_says_what_it_needs_and_why_it_failed() {
    let mut slack = FakeSlack::start().await;
    let fake = slack_host(&slack).await;
    let mut conn = fake.host.conn().await;
    let view = conn
        .settings_set(SettingsUpdate {
            secrets: [(
                "SLACK_BOT_TOKEN".to_owned(),
                Some("xoxb-1-SLACKTESTTOKEN".to_owned()),
            )]
            .into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let s = view.slack.unwrap();
    assert_eq!(s.state, "off");
    let detail = s.detail.unwrap();
    assert!(
        detail.contains("app token") && detail.contains("member ID"),
        "{detail}"
    );

    let err = conn
        .settings_set(SettingsUpdate {
            secrets: [(
                "SLACK_APP_TOKEN".to_owned(),
                Some("xoxb-1-SLACKTESTTOKEN".to_owned()),
            )]
            .into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("xapp-"), "{err:#}");

    conn.settings_set(set_up("xoxb-bad")).await.unwrap();
    let s = eventually("Slack refused the bot token", || async {
        let s = fake.host.conn().await.settings_get().await.unwrap().slack?;
        (s.state == "failed").then_some(s)
    })
    .await;
    assert_eq!(
        s.detail.as_deref(),
        Some("Slack said `invalid_auth` (the bot token)")
    );

    let fixed = conn
        .settings_set(set_up("xoxb-1-SLACKTESTTOKEN"))
        .await
        .unwrap()
        .slack
        .unwrap();
    assert_eq!(fixed.state, "connecting", "{fixed:?}");
    assert_eq!(fixed.detail, None);
    let _ws = slack.connected().await;
    eventually("connected once the token is fixed", || async {
        let s = fake.host.conn().await.settings_get().await.unwrap().slack?;
        (s.state == "connected").then_some(())
    })
    .await;
}

/// A button pressed on one of Otter's messages, as Socket Mode sends it,
/// and otterd's ack.
async fn press(
    ws: &mut WebSocketStream<TcpStream>,
    envelope: &str,
    user: &str,
    action: &str,
    value: &str,
    at: &str,
    on: &str,
) {
    let payload = json!({
        "type": "block_actions",
        "user": {"id": user},
        "container": {"type": "message", "message_ts": on, "channel_id": "D1"},
        "actions": [{"action_id": action, "value": value, "action_ts": at}],
    });
    let frame = json!({"type": "interactive", "envelope_id": envelope, "payload": payload});
    acked(ws, frame.to_string(), envelope).await;
}

/// The messages that told a need: top-level, with what they said.
fn told(slack: &FakeSlack) -> Vec<Value> {
    slack
        .calls("chat.postMessage")
        .into_iter()
        .map(|c| c.2)
        .filter(|b| b.get("thread_ts").is_none())
        .collect()
}

/// A task that needs the developer is told in Slack, once, with the app's
/// buttons (D-201). What waited before Slack connected isn't news. A reply
/// in the need's thread is said in that task's conversation, not Otter's on
/// the host, and Otter's answer comes back into the thread. Approve pressed
/// by someone not listed does nothing; by the developer it approves, once,
/// however often it is pressed, and the message then says so without its
/// buttons. With telling turned off a new need isn't told — nor after it is
/// turned on again.
#[tokio::test]
async fn a_task_that_needs_you_is_told_in_slack_and_answered_there() {
    use otter_core::task::{Decider, DecisionKind, DecisionStatus, Task};
    let mut slack = FakeSlack::start().await;
    let api = slack.api.clone();
    let fake = fake_agent_host_with("ok", "scripted", move |_| {
        vec![
            ("OTTER_BRAIN_V2", "1".into()),
            ("OTTER_SLACK_API", api),
            // Planning asks the developer to approve the plan.
            ("OTTER_PLAN_TURN", "1".into()),
        ]
    })
    .await;
    let host = &fake.host;
    let asks = |f: &Task| {
        f.pending_decisions()
            .any(|d| d.kind == DecisionKind::PlanApproval)
    };

    // Waiting before Slack was set up: not news.
    let before = started_task_named(host, "Add CSV export").await;
    wait_task(host, &before, "asking about its plan", asks).await;
    host.conn()
        .await
        .settings_set(set_up("xoxb-1-SLACKTESTTOKEN"))
        .await
        .unwrap();
    let mut ws = slack.connected().await;
    let state = host.home().join("state/slack.json");
    eventually("the needs looked at", || async {
        std::fs::read_to_string(&state)
            .ok()
            .filter(|s| s.contains("\"needs\""))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(told(&slack).is_empty(), "{:#?}", told(&slack));

    // A new need: told once, in the developer's direct messages.
    let id = started_task_named(host, "Add JSON export").await;
    let f = wait_task(host, &id, "asking about its plan", asks).await;
    let decision = f.pending_decisions().next().unwrap().id.clone();
    let need = eventually("the need told in Slack", || async {
        let t = told(&slack);
        (t.len() == 1).then(|| t[0].clone())
    })
    .await;
    assert_eq!(slack.calls("conversations.open")[0].2["users"], "U01DEV");
    assert_eq!(need["channel"], "D1");
    let text = need["blocks"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with(&format!("**{}** asks you to decide:", f.title)),
        "{text}"
    );
    let buttons = need["blocks"][1]["elements"].as_array().unwrap();
    let ids: Vec<&str> = buttons
        .iter()
        .map(|b| b["action_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["approve", "deny"]);
    let value = buttons[0]["value"].as_str().unwrap().to_owned();
    let on = "1800000000.000001";

    // A reply in its thread: said in the task, answered in the thread.
    let reply = dm(
        "e1",
        "U01DEV",
        "1700000005.000100",
        "Keep it small",
        json!({"thread_ts": on}),
    );
    acked(&mut ws, reply, "e1").await;
    let f = wait_task(host, &id, "the reply in the task", |f| {
        f.messages.iter().any(|m| {
            m.role == MessageRole::Controller
                && m.correlation_id.is_some()
                && f.messages.iter().any(|u| {
                    u.text == "Keep it small" && Some(u.id.as_str()) == m.correlation_id.as_deref()
                })
        })
    })
    .await;
    let mine = f
        .messages
        .iter()
        .find(|m| m.text == "Keep it small")
        .unwrap();
    assert_eq!(
        mine.correlation_id.as_deref(),
        Some("slack:D1:1700000005.000100:1800000000.000001")
    );
    let chat = host.conn().await.otter_get().await.unwrap();
    assert_eq!(said(&chat, "Keep it small"), 0);
    let answer = f
        .messages
        .iter()
        .find(|m| m.correlation_id.as_deref() == Some(mine.id.as_str()))
        .unwrap()
        .text
        .clone();
    // In the thread as it is written, then the answer in its place.
    let shown = eventually("Otter writing in the thread", || async {
        slack
            .calls("chat.postMessage")
            .into_iter()
            .find(|c| c.2["thread_ts"] == on)
    })
    .await;
    let n = slack
        .calls("chat.postMessage")
        .iter()
        .position(|c| c.2 == shown.2)
        .unwrap()
        + 1;
    let at = format!("1800000000.{n:06}");
    eventually("the answer in its place", || async {
        slack
            .calls("chat.update")
            .into_iter()
            .find(|c| c.2["ts"] == at.as_str() && c.2["blocks"][0]["text"] == answer.as_str())
    })
    .await;

    // Someone else's press: nothing.
    press(
        &mut ws,
        "p1",
        "U02OTHER",
        "approve",
        &value,
        "1700000006.1",
        on,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert!(f.pending_decisions().any(|d| d.id == decision));

    // The developer's: approved, and the message says so, without buttons.
    press(
        &mut ws,
        "p2",
        "U01DEV",
        "approve",
        &value,
        "1700000007.1",
        on,
    )
    .await;
    let f = wait_task(host, &id, "approved", |f| {
        f.decisions
            .iter()
            .any(|d| d.id == decision && d.status == DecisionStatus::Approved)
    })
    .await;
    let d = f.decisions.iter().find(|d| d.id == decision).unwrap();
    assert_eq!(d.decided_by, Some(Decider::User));
    let update = eventually("the need's message updated", || async {
        slack
            .calls("chat.update")
            .into_iter()
            .find(|c| c.2["ts"] == on)
    })
    .await;
    let blocks = update.2["blocks"].as_array().unwrap();
    assert!(blocks.iter().all(|b| b["type"] != "actions"), "{blocks:#?}");
    assert_eq!(blocks[1]["elements"][0]["text"], "✅ Approved by you.");

    // Pressed again (Slack sends a press once per tap): one command.
    press(
        &mut ws,
        "p3",
        "U01DEV",
        "approve",
        &value,
        "1700000007.1",
        on,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let refused = slack.calls("chat.postMessage").into_iter().any(|c| {
        c.2["blocks"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("didn't go through"))
    });
    assert!(!refused);

    // Telling turned off: a new need isn't told, nor once it's back on.
    let off = |on: bool| SettingsUpdate {
        slack: Some(SlackUpdate {
            notify: Some(on),
            ..Default::default()
        }),
        ..Default::default()
    };
    let view = host.conn().await.settings_set(off(false)).await.unwrap();
    assert_eq!(view.slack.unwrap().notify, Some(false));
    let quiet = started_task_named(host, "Add XML export").await;
    wait_task(host, &quiet, "asking about its plan", asks).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    host.conn().await.settings_set(off(true)).await.unwrap();
    // (No new connection for that: the same socket goes on.)
    acked(
        &mut ws,
        dm(
            "e9",
            "U01DEV",
            "1700000009.000100",
            "still here?",
            json!({}),
        ),
        "e9",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // (The approved task goes on, and may need the developer again: that
    // is told. The quiet one never is, nor the one from before.)
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    let posted: Vec<&str> = kept["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["task"].as_str().unwrap())
        .collect();
    assert!(posted.iter().all(|t| *t == id), "{posted:?}");
    assert!(kept["needs"][&quiet].is_string(), "{kept}");
}

/// `/otter` as Socket Mode sends it; otterd's ack, which carries the answer.
async fn slash(
    ws: &mut WebSocketStream<TcpStream>,
    envelope: &str,
    user: &str,
    text: &str,
) -> Value {
    let payload = json!({
        "command": "/otter", "text": text, "user_id": user, "channel_id": "D1",
        // One typing of it: Slack sends it again, the same, if the ack is late.
        "trigger_id": format!("trigger-{text}"),
    });
    let frame = json!({"type": "slash_commands", "envelope_id": envelope, "payload": payload});
    ws.send(Frame::text(frame.to_string())).await.unwrap();
    let ack = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("an ack")
        .unwrap()
        .unwrap();
    let ack: Value = serde_json::from_str(ack.to_text().unwrap()).unwrap();
    assert_eq!(ack["envelope_id"], envelope);
    ack["payload"].clone()
}

/// `/otter` (D-202): `status` lists what needs the developer; someone not
/// listed is refused; anything unknown gets the help; `new` makes a task
/// of its words and starts it, with a thread of its own whose first word is
/// the request, so what Otter says about it comes back there, and a reply
/// in it goes to the task.
#[tokio::test]
async fn otter_in_slack_says_what_needs_you_and_starts_a_task() {
    use otter_core::task::{DecisionKind, Task, TaskStatus};
    let mut slack = FakeSlack::start().await;
    let api = slack.api.clone();
    let fake = fake_agent_host_with("ok", "scripted", move |_| {
        vec![
            ("OTTER_BRAIN_V2", "1".into()),
            ("OTTER_SLACK_API", api),
            ("OTTER_PLAN_TURN", "1".into()),
        ]
    })
    .await;
    let host = &fake.host;
    let asks = |f: &Task| {
        f.pending_decisions()
            .any(|d| d.kind == DecisionKind::PlanApproval)
    };
    let waiting = started_task_named(host, "Add CSV export").await;
    let f = wait_task(host, &waiting, "asking about its plan", asks).await;
    host.conn()
        .await
        .settings_set(set_up("xoxb-1-SLACKTESTTOKEN"))
        .await
        .unwrap();
    let mut ws = slack.connected().await;

    let status = slash(&mut ws, "s1", "U01DEV", "status").await;
    assert_eq!(status["response_type"], "ephemeral");
    let text = status["blocks"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("**Needs you** (1)"), "{text}");
    assert!(text.contains(&format!("**{}**", f.title)), "{text}");
    let refused = slash(&mut ws, "s2", "U02OTHER", "status").await;
    assert!(
        refused["text"]
            .as_str()
            .unwrap()
            .contains("only the developer"),
        "{refused}"
    );
    let help = slash(&mut ws, "s3", "U01DEV", "deploy it").await;
    assert!(
        help["blocks"][0]["text"]
            .as_str()
            .unwrap()
            .contains("/otter new")
    );

    // A new task, started, in a thread of its own.
    let started = slash(&mut ws, "s4", "U01DEV", "new Add XML export\nwith tests").await;
    assert!(
        started["text"]
            .as_str()
            .unwrap()
            .starts_with("Starting **Add XML export**"),
        "{started}"
    );
    let head = eventually("the task's message", || async {
        slack
            .calls("chat.postMessage")
            .into_iter()
            .map(|c| c.2)
            .find(|b| b["text"] == "Add XML export")
    })
    .await;
    // Its time: each message's is its place among those posted.
    let n = slack
        .calls("chat.postMessage")
        .iter()
        .position(|c| c.2["text"] == "Add XML export")
        .unwrap()
        + 1;
    let ts = format!("1800000000.{n:06}");
    assert!(head.get("thread_ts").is_none());
    // Sent again (its ack was late): answered, not done again.
    slash(&mut ws, "s5", "U01DEV", "new Add XML export\nwith tests").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let tasks = host.conn().await.task_list().await.unwrap();
    let made: Vec<_> = tasks
        .iter()
        .filter(|t| t.title == "Add XML export")
        .collect();
    assert_eq!(made.len(), 1, "{made:#?}");
    let task = made[0].clone();
    let request = task
        .messages
        .iter()
        .find(|m| m.role == otter_core::task::MessageRole::User)
        .unwrap();
    assert_eq!(request.text, "Add XML export\nwith tests");
    assert_eq!(
        request.correlation_id.as_deref(),
        Some(format!("slack:D1:{ts}:{ts}").as_str())
    );
    wait_task(host, &task.id.to_string(), "started", |f| {
        f.status != TaskStatus::Draft
    })
    .await;
    // What Otter says about it comes back into its thread.
    eventually("Otter in the task's thread", || async {
        slack
            .calls("chat.postMessage")
            .into_iter()
            .find(|c| c.2["thread_ts"] == ts.as_str())
    })
    .await;
    // And a reply there is the task's.
    acked(
        &mut ws,
        dm(
            "e1",
            "U01DEV",
            "1700000010.000100",
            "Use the xml crate",
            json!({"thread_ts": ts}),
        ),
        "e1",
    )
    .await;
    wait_task(host, &task.id.to_string(), "the reply in the task", |f| {
        f.messages.iter().any(|m| m.text == "Use the xml crate")
    })
    .await;
}

/// A form (D-202): a decision of several questions is asked with a select
/// per question; what a select chose as it was chosen and what the message
/// holds at Submit are its answers, done as the app's form does it. A
/// Submit with a question unanswered says so and decides nothing.
#[tokio::test]
async fn a_form_is_answered_in_slack() {
    use otter_core::task::{Decider, DecisionStatus, TaskStatus};
    let mut slack = FakeSlack::start().await;
    let api = slack.api.clone();
    let fake = fake_agent_host_with("ok", "rules", move |_| {
        vec![("OTTER_BRAIN_V2", "1".into()), ("OTTER_SLACK_API", api)]
    })
    .await;
    let host = &fake.host;
    host.conn()
        .await
        .settings_set(set_up("xoxb-1-SLACKTESTTOKEN"))
        .await
        .unwrap();
    let mut ws = slack.connected().await;
    let state = host.home().join("state/slack.json");
    eventually("the needs looked at", || async {
        std::fs::read_to_string(&state)
            .ok()
            .filter(|s| s.contains("\"needs\""))
    })
    .await;

    let id = started_task_named(host, "Set up ASK_FORM").await;
    let f = wait_task(host, &id, "asking its questions", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let decision = f.pending_decisions().next().unwrap().id.clone();
    let need = eventually("the form told in Slack", || async {
        told(&slack).into_iter().find(|b| {
            b["blocks"].as_array().is_some_and(|bs| {
                bs.iter()
                    .any(|x| x["block_id"].as_str().is_some_and(|i| i.starts_with("q:")))
            })
        })
    })
    .await;
    let blocks = need["blocks"].as_array().unwrap();
    let questions: Vec<&Value> = blocks
        .iter()
        .filter(|b| b["block_id"].as_str().is_some_and(|i| i.starts_with("q:")))
        .collect();
    assert_eq!(questions.len(), 2, "{blocks:#?}");
    assert_eq!(questions[0]["elements"][0]["type"], "static_select");
    assert_eq!(questions[1]["elements"][0]["type"], "multi_static_select");
    let (first, second) = (
        questions[0]["block_id"].as_str().unwrap().to_owned(),
        questions[1]["block_id"].as_str().unwrap().to_owned(),
    );
    let submit = blocks
        .iter()
        .flat_map(|b| b["elements"].as_array().cloned().unwrap_or_default())
        .find(|e| e["action_id"] == "form")
        .unwrap();
    let value = submit["value"].as_str().unwrap().to_owned();
    let on = format!(
        "1800000000.{:06}",
        slack
            .calls("chat.postMessage")
            .iter()
            .position(|c| c.2 == need)
            .unwrap()
            + 1
    );
    let send = |envelope: &str, payload: Value| {
        json!({"type": "interactive", "envelope_id": envelope, "payload": payload}).to_string()
    };
    let container = json!({"type": "message", "message_ts": on, "channel_id": "D1"});

    // The first question chosen as it is chosen.
    let pick = json!({
        "type": "block_actions", "user": {"id": "U01DEV"}, "container": container,
        "actions": [{"action_id": "pick", "block_id": first, "action_ts": "1700000020.1",
            "type": "static_select", "selected_option": {"value": "SQLite"}}],
    });
    acked(&mut ws, send("f1", pick), "f1").await;

    // Submit with the second unanswered: said, nothing decided.
    let early = json!({
        "type": "block_actions", "user": {"id": "U01DEV"}, "container": container,
        "actions": [{"action_id": "form", "value": value, "action_ts": "1700000021.1"}],
        "state": {"values": {}},
    });
    acked(&mut ws, send("f2", early), "f2").await;
    eventually("what's still to answer", || async {
        slack.calls("chat.postMessage").into_iter().find(|c| {
            c.2["thread_ts"] == on.as_str()
                && c.2["blocks"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("Still to answer"))
        })
    })
    .await;
    let f = host.conn().await.task_get(&id).await.unwrap();
    assert!(f.pending_decisions().any(|d| d.id == decision));

    // Submit with the second in the message's state: answered.
    let state_values = json!({ second.clone(): {"pick": {"type": "multi_static_select",
        "selected_options": [{"value": "Rust"}, {"value": "Go"}]}} });
    let submit = json!({
        "type": "block_actions", "user": {"id": "U01DEV"}, "container": container,
        "actions": [{"action_id": "form", "value": value, "action_ts": "1700000022.1"}],
        "state": {"values": state_values},
    });
    acked(&mut ws, send("f3", submit), "f3").await;
    let f = wait_task(host, &id, "answered", |f| {
        f.decisions
            .iter()
            .any(|d| d.id == decision && d.status == DecisionStatus::Answered)
    })
    .await;
    let d = f.decisions.iter().find(|d| d.id == decision).unwrap();
    assert_eq!(d.decided_by, Some(Decider::User));
    let answer = d.answer.clone().unwrap();
    let (q1, q2) = (&first["q:".len()..], &second["q:".len()..]);
    assert_eq!(answer, format!("{q1} SQLite; {q2} Rust, Go"), "{answer}");
}

/// Pictures (D-202): a picture sent to the app reaches Otter's turn — a
/// thumbnail where the picture itself is too large — and a file that isn't
/// one is said to be left out.
#[tokio::test]
async fn a_picture_in_slack_reaches_otter() {
    let mut slack = FakeSlack::start().await;
    let api = slack.api.clone();
    let fake = fake_agent_host_with("ok", "scripted", move |_| {
        vec![("OTTER_BRAIN_V2", "1".into()), ("OTTER_SLACK_API", api)]
    })
    .await;
    let host = &fake.host;
    host.conn()
        .await
        .settings_set(set_up("xoxb-1-SLACKTESTTOKEN"))
        .await
        .unwrap();
    let mut ws = slack.connected().await;
    let base = slack.api.trim_end_matches("/api").to_owned();
    let files = json!([
        {"mimetype": "image/png", "size": 2_000_000,
         "url_private_download": format!("{base}/files/big.png"),
         "thumb_720": format!("{base}/files/small.png")},
        {"mimetype": "application/pdf", "size": 10,
         "url_private_download": format!("{base}/files/doc.pdf")},
    ]);
    let event = dm(
        "e1",
        "U01DEV",
        "1700000030.000100",
        "PICTURES",
        json!({"subtype": "file_share", "files": files}),
    );
    acked(&mut ws, event, "e1").await;
    let chat = eventually("the message with its picture", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        c.messages
            .iter()
            .any(|m| m.text == "PICTURES" && m.attachments.len() == 1)
            .then_some(c)
    })
    .await;
    assert_eq!(said(&chat, "PICTURES"), 1);
    // The thumbnail, with the bot's token; never the picture too large.
    let fetched: Vec<Value> = slack.calls("file").into_iter().map(|c| c.2).collect();
    assert_eq!(fetched, vec![json!("small.png")], "{fetched:?}");
    assert_eq!(slack.calls("file")[0].1, "Bearer xoxb-1-SLACKTESTTOKEN");
    let said_what = |what: &'static str| {
        let slack = &slack;
        async move {
            slack.calls("chat.postMessage").into_iter().find(|c| {
                c.2["blocks"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(what))
            })
        }
    };
    eventually("the PDF said to be left out", || {
        said_what("wasn't passed on")
    })
    .await;
    eventually("Otter shown the picture", || {
        said_what("Shown: [image/png]")
    })
    .await;
}

/// An answer shown as it is written is finished where it is (D-202), even
/// when the developer wrote elsewhere meanwhile, so the answer itself goes
/// nowhere in Slack: its words aren't left half written.
#[tokio::test]
async fn an_answer_shown_as_it_is_written_is_finished_where_it_is() {
    let mut slack = FakeSlack::start().await;
    let api = slack.api.clone();
    let fake = fake_agent_host_with("ok", "scripted", move |_| {
        vec![("OTTER_BRAIN_V2", "1".into()), ("OTTER_SLACK_API", api)]
    })
    .await;
    let host = &fake.host;
    host.conn()
        .await
        .settings_set(set_up("xoxb-1-SLACKTESTTOKEN"))
        .await
        .unwrap();
    let mut ws = slack.connected().await;
    // Otter takes a while over it; meanwhile the developer writes from the
    // terminal.
    acked(
        &mut ws,
        dm(
            "e1",
            "U01DEV",
            "1700000040.000100",
            "SLEEP: 1500",
            json!({}),
        ),
        "e1",
    )
    .await;
    eventually("Otter at it", || async {
        let c = host.conn().await.otter_get().await.unwrap();
        (said(&c, "SLEEP: 1500") == 1).then_some(())
    })
    .await;
    host.conn()
        .await
        .otter_send("cli-1", "and meanwhile")
        .await
        .unwrap();
    let shown = eventually("the answer shown as it is written", || async {
        slack
            .calls("chat.postMessage")
            .into_iter()
            .find(|c| c.2["blocks"][0]["text"] == "Scripted: None. …")
    })
    .await;
    let n = slack
        .calls("chat.postMessage")
        .iter()
        .position(|c| c.2 == shown.2)
        .unwrap()
        + 1;
    let at = format!("1800000000.{n:06}");
    eventually("finished where it is", || async {
        slack
            .calls("chat.update")
            .into_iter()
            .find(|c| c.2["ts"] == at.as_str() && c.2["blocks"][0]["text"] == "Scripted: None.")
    })
    .await;
}

/// The coding agent's words as it says them (D-203): in its task's thread,
/// while the developer last spoke there, one message for its turn — what it
/// says as it says it, then the message its turn ends with in its place.
#[tokio::test]
async fn the_coding_agents_words_are_shown_as_it_says_them() {
    use otter_core::task::TaskStatus;
    let mut slack = FakeSlack::start().await;
    let api = slack.api.clone();
    let fake = fake_agent_host_with("ok", "rules", move |scratch| {
        let worker = scratch.join("worker.sh");
        std::fs::write(&worker, include_str!("../fake_claude_worker.sh")).unwrap();
        vec![
            ("OTTER_CODING_BACKEND", "sdk".into()),
            ("OTTER_NODE", "/bin/sh".into()),
            ("OTTER_CLAUDE_WORKER", worker.to_string_lossy().into_owned()),
            (
                "FAKE_WORKER_LOG",
                scratch.join("worker.log").to_string_lossy().into_owned(),
            ),
            ("FAKE_WORKER_MODE", "ok".into()),
            ("OTTER_SLACK_API", api),
        ]
    })
    .await;
    let host = &fake.host;
    host.conn()
        .await
        .settings_set(set_up("xoxb-1-SLACKTESTTOKEN"))
        .await
        .unwrap();
    let mut ws = slack.connected().await;
    let state = host.home().join("state/slack.json");
    eventually("the needs looked at", || async {
        std::fs::read_to_string(&state)
            .ok()
            .filter(|s| s.contains("\"needs\""))
    })
    .await;

    // The agent asks its questions: told in Slack, as a form.
    let id = started_task_named(host, "Add an export").await;
    wait_task(host, &id, "asking", |f| {
        f.status == TaskStatus::Blocked && f.pending_decisions().next().is_some()
    })
    .await;
    let need = eventually("the form told in Slack", || async {
        told(&slack).into_iter().find(|b| {
            b["blocks"].as_array().is_some_and(|bs| {
                bs.iter()
                    .any(|x| x["block_id"].as_str().is_some_and(|i| i.starts_with("q:")))
            })
        })
    })
    .await;
    let on = format!(
        "1800000000.{:06}",
        slack
            .calls("chat.postMessage")
            .iter()
            .position(|c| c.2 == need)
            .unwrap()
            + 1
    );
    // The developer speaks in its thread, then answers the form.
    acked(
        &mut ws,
        dm(
            "e1",
            "U01DEV",
            "1700000050.000100",
            "Keep going",
            json!({"thread_ts": on}),
        ),
        "e1",
    )
    .await;
    wait_task(host, &id, "the reply in the task", |f| {
        f.messages.iter().any(|m| m.text == "Keep going")
    })
    .await;
    let blocks = need["blocks"].as_array().unwrap();
    let mut values = serde_json::Map::new();
    for q in blocks
        .iter()
        .filter(|b| b["block_id"].as_str().is_some_and(|i| i.starts_with("q:")))
    {
        let first = q["elements"][0]["options"][0]["value"].clone();
        values.insert(
            q["block_id"].as_str().unwrap().to_owned(),
            json!({"pick": {"selected_option": {"value": first}, "selected_options": [{"value": first}]}}),
        );
    }
    let value = blocks
        .iter()
        .flat_map(|b| b["elements"].as_array().cloned().unwrap_or_default())
        .find(|e| e["action_id"] == "form")
        .unwrap()["value"]
        .as_str()
        .unwrap()
        .to_owned();
    let submit = json!({"type": "interactive", "envelope_id": "f1", "payload": {
        "type": "block_actions", "user": {"id": "U01DEV"},
        "container": {"type": "message", "message_ts": on, "channel_id": "D1"},
        "actions": [{"action_id": "form", "value": value, "action_ts": "1700000051.1"}],
        "state": {"values": values},
    }});
    acked(&mut ws, submit.to_string(), "f1").await;

    // What it says as it says it, in the thread; then its turn's message in
    // that message's place.
    let shown = eventually("the agent's words in the thread", || async {
        slack
            .calls("chat.postMessage")
            .into_iter()
            .find(|c| c.2["thread_ts"] == on.as_str() && c.2["blocks"][0]["text"] == "Do …")
    })
    .await;
    let n = slack
        .calls("chat.postMessage")
        .iter()
        .position(|c| c.2 == shown.2)
        .unwrap()
        + 1;
    let at = format!("1800000000.{n:06}");
    wait_task(host, &id, "the agent's message", |f| {
        f.messages
            .iter()
            .any(|m| m.role == MessageRole::Agent && m.text == "Done.")
    })
    .await;
    eventually("its message in their place", || async {
        slack
            .calls("chat.update")
            .into_iter()
            .find(|c| c.2["ts"] == at.as_str() && c.2["blocks"][0]["text"] == "Done.")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let again = slack
        .calls("chat.postMessage")
        .into_iter()
        .any(|c| c.2["thread_ts"] == on.as_str() && c.2["blocks"][0]["text"] == "Done.");
    assert!(!again, "the agent's message was posted again");
}
