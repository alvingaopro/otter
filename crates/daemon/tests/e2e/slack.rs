//! Otter in Slack (D-198): a direct message to the Slack app is said in
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
        tokio::spawn(async move {
            while let Ok((s, _)) = http.accept().await {
                tokio::spawn(answer(s, link.clone(), kept.clone()));
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
async fn answer(mut s: TcpStream, link: String, calls: Arc<Mutex<Vec<Call>>>) {
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
        "chat.postMessage" => json!({"ok": true, "ts": "1800000000.000100"}),
        "reactions.add" => json!({"ok": true}),
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
    assert_eq!(body["blocks"][0]["text"], reply.text.as_str());
    assert!(body.get("thread_ts").is_none(), "{body}");
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

/// Settings say what Slack still needs, refuse the tokens swapped, and say
/// why a connection failed.
#[tokio::test]
async fn slack_says_what_it_needs_and_why_it_failed() {
    let slack = FakeSlack::start().await;
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
    assert!(s.detail.unwrap().contains("invalid_auth"));
}
