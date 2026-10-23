//! Otter in Slack (D-198): the developer's direct messages to the Slack app
//! set on this host are said in Otter's conversation here (D-093), and
//! Otter's messages there go back to Slack while the developer last spoke
//! from it.
//!
//! Slack's Socket Mode: otterd opens a WebSocket to Slack — outbound, like
//! its calls to model providers; nothing listens (D-003) — and acks each
//! event Slack sends down it. The Web API goes through curl (`api.rs`).
//!
//! ```text
//! state/slack.json   { "after": "msg_…" }   the last message looked at for Slack
//! ```
//!
//! One host per Slack app: Slack shares a connection's events among every
//! connection the app has open, so two hosts would each hear some of them.

mod api;
mod socket;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use otter_core::task::{Message, MessageRole};
use otter_protocol::Event;
use otter_protocol::host::SlackView;
use otter_protocol::otter::OtterSend;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as Frame;

use crate::daemon::Daemon;
use crate::env::EnvMap;
use crate::settings::HostSettings;
use api::CallError;
use socket::{Envelope, Origin};

/// The Slack app's bot token: reads and writes its messages.
pub const SLACK_BOT_TOKEN: &str = "SLACK_BOT_TOKEN";
/// The Slack app's app-level token (`connections:write`): opens the socket.
pub const SLACK_APP_TOKEN: &str = "SLACK_APP_TOKEN";

/// Slack's limit for the Markdown in one message.
const MAX_PIECE: usize = 11_000;
/// Silence on the socket longer than this is a dead connection (Slack pings
/// far more often).
const QUIET: Duration = Duration::from_secs(150);
/// Otter's messages older than this aren't sent when Slack comes back.
const STALE: chrono::TimeDelta = chrono::TimeDelta::hours(6);

/// The connection's state, for Settings.
#[derive(Default)]
pub struct Slack {
    status: std::sync::Mutex<Status>,
    restart: tokio::sync::Notify,
}

#[derive(Clone, Debug, Default)]
struct Status {
    /// `connecting`, `connected` or `failed`; `None`: not started.
    state: Option<&'static str>,
    detail: Option<String>,
    team: Option<String>,
    bot: Option<String>,
}

impl Slack {
    /// The settings changed: connect again with them. Said at once, so
    /// Settings' answer to the change doesn't show the last connection's
    /// failure.
    pub fn restart(&self) {
        *self.status.lock().unwrap() = Status {
            state: Some("connecting"),
            ..Default::default()
        };
        self.restart.notify_one();
    }
}

/// What connecting takes, as set in Settings.
struct Config {
    bot: String,
    app: String,
    users: Vec<String>,
}

/// The config, or what is missing from it.
fn config(s: &HostSettings) -> Result<Config, String> {
    let (bot, app) = (s.secret(SLACK_BOT_TOKEN), s.secret(SLACK_APP_TOKEN));
    let users = s.slack_users().to_vec();
    let mut missing = Vec::new();
    if bot.is_none() {
        missing.push("the bot token (xoxb-…)");
    }
    if app.is_none() {
        missing.push("the app token (xapp-…)");
    }
    if users.is_empty() {
        missing.push("your Slack member ID");
    }
    match (bot, app, missing.is_empty()) {
        (Some(bot), Some(app), true) => Ok(Config { bot, app, users }),
        _ => Err(format!("Add {}.", missing.join(", "))),
    }
}

/// Slack members as Settings takes them: member IDs (`U…`, `W…`), each once.
pub fn members(users: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for u in users.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
        let id = u.trim_start_matches('@');
        let ok = id.len() >= 3
            && id.len() <= 32
            && (id.starts_with('U') || id.starts_with('W'))
            && id
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if !ok {
            return Err(format!(
                "`{u}` isn't a Slack member ID (U…: in Slack, your profile › ⋯ › Copy member ID)"
            ));
        }
        if !out.iter().any(|o| o == id) {
            out.push(id.to_owned());
        }
    }
    if out.len() > 20 {
        return Err("at most 20 Slack members".into());
    }
    Ok(out)
}

/// What was sent to Slack last, kept across restarts.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Sent {
    /// The last of the conversation's messages looked at for Slack: sent,
    /// or not Slack's. `None`: from the start.
    #[serde(default)]
    after: Option<String>,
}

impl Daemon {
    /// Settings' view of Slack: what is missing at once, else how the
    /// connection is.
    pub(crate) fn slack_view(&self) -> SlackView {
        let s = self.settings.read().unwrap();
        let users = s.slack_users().to_vec();
        if let Err(missing) = config(&s) {
            return SlackView {
                users,
                state: "off".into(),
                detail: Some(missing),
                ..Default::default()
            };
        }
        let st = self.slack.status.lock().unwrap().clone();
        SlackView {
            users,
            state: st.state.unwrap_or("connecting").into(),
            detail: st.detail,
            team: st.team,
            bot: st.bot,
        }
    }

    fn slack_status(&self, status: Status) {
        *self.slack.status.lock().unwrap() = status;
    }

    /// Keep Slack connected while it is set up here: again after Slack
    /// closes it, after a failure (waiting longer each time, up to two
    /// minutes) and at once when the settings change.
    pub(crate) async fn run_slack(self: Arc<Self>) {
        let mut shutdown = self.shutdown_signal();
        let mut wait = Duration::ZERO;
        loop {
            let config = config(&self.settings.read().unwrap());
            match config {
                Err(_) => {
                    self.slack_status(Status::default());
                    self.clear_problem("slack");
                    tokio::select! {
                        _ = self.slack.restart.notified() => {}
                        _ = shutdown.changed() => return,
                    }
                    wait = Duration::ZERO;
                    continue;
                }
                Ok(c) => {
                    self.slack_status(Status {
                        state: Some("connecting"),
                        ..Default::default()
                    });
                    let ended = tokio::select! {
                        r = self.slack_session(&c) => r,
                        _ = self.slack.restart.notified() => {
                            wait = Duration::ZERO;
                            continue;
                        }
                        _ = shutdown.changed() => return,
                    };
                    match ended {
                        // Slack closed it: open another.
                        Ok(()) => wait = Duration::from_secs(1),
                        Err(e) => {
                            // A connection that worked starts the waiting over.
                            let worked =
                                self.slack.status.lock().unwrap().state == Some("connected");
                            if worked {
                                wait = Duration::ZERO;
                            }
                            let why = otter_core::redact::redact(&format!("{e:#}"));
                            tracing::warn!("slack: {why}");
                            self.note_problem(
                                "slack",
                                format!("Otter isn't connected to Slack: {why}."),
                                "Check the Slack app's two tokens and Socket Mode in Settings › Slack; Otter tries again meanwhile.".into(),
                            );
                            self.slack_status(Status {
                                state: Some("failed"),
                                detail: Some(why),
                                ..Default::default()
                            });
                            wait =
                                (wait * 2).clamp(Duration::from_secs(2), Duration::from_secs(120));
                        }
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.slack.restart.notified() => wait = Duration::ZERO,
                _ = shutdown.changed() => return,
            }
        }
    }

    /// One connection, until Slack closes it (`Ok`) or it fails.
    async fn slack_session(self: &Arc<Self>, c: &Config) -> anyhow::Result<()> {
        let env = self.environments.base().clone();
        let me = api::call(&env, &c.bot, "auth.test", &json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("{e} (the bot token)"))?;
        let open = api::call(&env, &c.app, "apps.connections.open", &json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("{e} (the app token)"))?;
        // Its address carries a ticket: never logged.
        let url = open["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Slack gave no address to connect to"))?;
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| anyhow::anyhow!("connecting to Slack: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (_, mut events) = self.events.subscribe();
        let wake = tokio::sync::Notify::new();
        // Otter's messages go out apart from reading, so an ack never waits
        // on a post.
        let mirror = async {
            loop {
                wake.notified().await;
                match self.slack_mirror(c, &env).await {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::warn!("slack: sending Otter's messages: {e}");
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        wake.notify_one();
                    }
                }
            }
        };
        let read = async {
            // Silence is measured from Slack's last frame, whatever else
            // happens on the host meanwhile.
            let quiet = tokio::time::sleep(QUIET);
            tokio::pin!(quiet);
            loop {
                tokio::select! {
                    () = &mut quiet => anyhow::bail!("Slack went quiet"),
                    frame = stream.next() => {
                        quiet.as_mut().reset(tokio::time::Instant::now() + QUIET);
                        let text = match frame {
                            None | Some(Ok(Frame::Close(_))) => return Ok(()),
                            Some(Err(e)) => anyhow::bail!("the connection to Slack: {e}"),
                            Some(Ok(Frame::Text(t))) => t,
                            Some(Ok(_)) => continue,
                        };
                        match socket::envelope(text.as_str()) {
                            Envelope::Hello => {
                                tracing::info!("slack: connected");
                                self.clear_problem("slack");
                                self.slack_status(Status {
                                    state: Some("connected"),
                                    detail: None,
                                    team: me["team"].as_str().map(String::from),
                                    bot: me["user"].as_str().map(String::from),
                                });
                                wake.notify_one();
                            }
                            Envelope::Disconnect(why) => {
                                tracing::info!("slack: Slack closed the connection ({why})");
                                return Ok(());
                            }
                            Envelope::Event { envelope_id, event } => {
                                let ack = json!({ "envelope_id": envelope_id }).to_string();
                                sink.send(Frame::text(ack)).await?;
                                self.slack_heard(c, &env, &event).await;
                            }
                            Envelope::Other { envelope_id } => {
                                let ack = json!({ "envelope_id": envelope_id }).to_string();
                                sink.send(Frame::text(ack)).await?;
                            }
                            Envelope::Unknown => {}
                        }
                    }
                    ev = events.recv() => match ev {
                        Ok(r) if matches!(r.event, Event::OtterChanged { .. }) => wake.notify_one(),
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => wake.notify_one(),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                    },
                }
            }
        };
        tokio::select! {
            r = read => r,
            _ = mirror => Ok(()),
        }
    }

    /// An event from Slack: the developer's direct message is said in
    /// Otter's conversation, and marked seen (👀) once it is. What goes back
    /// to Slack goes apart from reading, so the next frame isn't held up.
    async fn slack_heard(&self, c: &Config, env: &EnvMap, event: &Value) {
        let m = match socket::inbound(event, &c.users) {
            Ok(m) => m,
            Err(socket::Skip::NotListed) => {
                tracing::info!("slack: a message from a Slack member Settings doesn't list");
                return;
            }
            Err(_) => return,
        };
        let sent = self
            .otter_send(OtterSend {
                command_id: m.command_id(),
                text: m.text.clone(),
            })
            .await;
        let (env, bot) = (env.clone(), c.bot.clone());
        tokio::spawn(async move {
            match sent {
                Ok(_) => {
                    let seen =
                        json!({ "channel": m.to.channel, "timestamp": m.ts, "name": "eyes" });
                    match api::call(&env, &bot, "reactions.add", &seen).await {
                        Ok(_) | Err(CallError::Refused(_)) => {}
                        Err(e) => tracing::warn!("slack: marking a message seen: {e}"),
                    }
                }
                Err(e) => {
                    let line = format!("Otter didn't take that: {}", e.message);
                    if let Err(e) = post(&env, &bot, &m.to, &line).await {
                        tracing::warn!("slack: {e}");
                    }
                }
            }
        });
    }

    /// Send Otter's messages that Slack hasn't had: each that comes after
    /// a message the developer wrote in Slack, to where they wrote it.
    async fn slack_mirror(&self, c: &Config, env: &EnvMap) -> Result<(), CallError> {
        let path = self.paths.state_dir.join("slack.json");
        let chat = self.chat.lock().await.get().clone();
        let save = |sent: &Sent| {
            let bytes = serde_json::to_vec(sent).expect("serializes");
            if let Err(e) = crate::settings::write_private(&path, &bytes) {
                tracing::warn!("slack: saving {}: {e:#}", path.display());
            }
        };
        // The first time (or a file that can't be read): what is there
        // already isn't sent.
        let mut sent: Sent = match std::fs::read(&path).map(|b| serde_json::from_slice(&b)) {
            Ok(Ok(sent)) => sent,
            _ => {
                let sent = Sent {
                    after: chat.messages.last().map(|m| m.id.to_string()),
                };
                save(&sent);
                sent
            }
        };
        let Some(from) = resume_at(&chat.messages, sent.after.as_deref()) else {
            // Gone from the conversation (it started over): from its end.
            save(&Sent {
                after: chat.messages.last().map(|m| m.id.to_string()),
            });
            return Ok(());
        };
        let now = chrono::Utc::now();
        for (i, m) in chat.messages.iter().enumerate().skip(from) {
            let to = (now - m.at < STALE)
                .then(|| destination(&chat.messages, i))
                .flatten();
            if let Some(to) = to {
                match post(env, &c.bot, &to, &m.text).await {
                    Ok(()) => {}
                    Err(e @ CallError::Unreachable(_)) => return Err(e),
                    // Never sent: said, and not tried again for ever.
                    Err(e) => tracing::warn!("slack: Otter's message {} wasn't sent: {e}", m.id),
                }
            }
            sent.after = Some(m.id.to_string());
            save(&sent);
        }
        Ok(())
    }
}

/// Where sending to Slack picks up in the conversation: after `after`, or
/// from the start with none; `None` when `after` is gone from it.
fn resume_at(messages: &[Message], after: Option<&str>) -> Option<usize> {
    match after {
        None => Some(0),
        Some(id) => messages
            .iter()
            .position(|m| m.id.as_str() == id)
            .map(|i| i + 1),
    }
}

/// Where the conversation's `i`th message goes in Slack: one of Otter's,
/// when the developer's latest message before it came from Slack.
fn destination(messages: &[Message], i: usize) -> Option<Origin> {
    if messages[i].role != MessageRole::Controller {
        return None;
    }
    let last = messages[..i]
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)?;
    Origin::of(last.correlation_id.as_deref())
}

/// Post `text` (Markdown) to `to`, in as many messages as it takes.
async fn post(env: &EnvMap, bot: &str, to: &Origin, text: &str) -> Result<(), CallError> {
    let text = otter_core::redact::redact(text);
    for piece in socket::pieces(&text, MAX_PIECE) {
        let mut body = json!({
            "channel": to.channel,
            // What a notification shows.
            "text": piece.chars().take(280).collect::<String>(),
            "blocks": [{ "type": "markdown", "text": piece }],
            "unfurl_links": false,
            "unfurl_media": false,
        });
        if let Some(t) = &to.thread {
            body["thread_ts"] = json!(t);
        }
        api::call(env, bot, "chat.postMessage", &body).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, role: MessageRole, from: Option<&str>) -> Message {
        Message {
            id: id.into(),
            role,
            text: id.into(),
            at: chrono::Utc::now(),
            correlation_id: from.map(String::from),
            attachments: vec![],
            command: None,
        }
    }

    /// Otter's messages go to Slack while the developer last spoke from
    /// it: an answer, and a follow-up after it; not once they speak from
    /// the terminal, and never the developer's own words.
    #[test]
    fn otters_messages_go_where_the_developer_last_spoke() {
        use MessageRole::{Controller, User};
        let m = vec![
            msg("m0", Controller, Some("follow:t1")),
            msg("m1", User, Some("slack:D1:1.0")),
            msg("m2", Controller, Some("m1")),
            msg("m3", Controller, Some("follow:t1")),
            msg("m4", User, Some("cli-1")),
            msg("m5", Controller, Some("m4")),
            msg("m6", User, Some("slack:D1:2.0:1.5")),
            msg("m7", Controller, Some("m6")),
        ];
        let to: Vec<Option<String>> = (0..m.len())
            .map(|i| destination(&m, i).map(|o| format!("{}{:?}", o.channel, o.thread)))
            .collect();
        assert_eq!(
            to,
            vec![
                None,
                None,
                Some("D1None".into()),
                Some("D1None".into()),
                None,
                None,
                None,
                Some("D1Some(\"1.5\")".into()),
            ]
        );
    }

    /// Where sending picks up: after the last message looked at, from the
    /// start with none, and nowhere when that message is gone (the
    /// conversation started over), so the caller starts from its end.
    #[test]
    fn sending_picks_up_after_the_last_message_looked_at() {
        let m = vec![
            msg("m0", MessageRole::User, None),
            msg("m1", MessageRole::Controller, None),
        ];
        assert_eq!(resume_at(&m, None), Some(0));
        assert_eq!(resume_at(&m, Some("m0")), Some(1));
        assert_eq!(resume_at(&m, Some("m1")), Some(2));
        assert_eq!(resume_at(&m, Some("gone")), None);
    }

    #[test]
    fn member_ids_are_taken_as_slack_writes_them() {
        let ids = |v: &[&str]| members(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            ids(&[" U06MPBPRX7C ", "@W012ABC", "U06MPBPRX7C", ""]).unwrap(),
            vec!["U06MPBPRX7C", "W012ABC"]
        );
        for bad in ["alvin", "u06mpbprx7c", "C123", "U1-2"] {
            let err = ids(&[bad]).unwrap_err();
            assert!(err.contains("Copy member ID"), "{bad}: {err}");
        }
        assert!(ids(&[]).unwrap().is_empty());
    }

    #[test]
    fn settings_say_what_slack_still_needs() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = HostSettings::load(dir.path()).unwrap();
        let err = config(&s).err().unwrap();
        assert!(
            err.contains("bot token") && err.contains("app token") && err.contains("member ID")
        );
        s.update(otter_protocol::host::SettingsUpdate {
            secrets: [
                (SLACK_BOT_TOKEN.to_owned(), Some("xoxb-1".to_owned())),
                (SLACK_APP_TOKEN.to_owned(), Some("xapp-1".to_owned())),
            ]
            .into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(config(&s).err().unwrap(), "Add your Slack member ID.");
        s.update(otter_protocol::host::SettingsUpdate {
            slack: Some(otter_protocol::host::SlackUpdate {
                users: Some(vec!["U1A".into()]),
            }),
            ..Default::default()
        })
        .unwrap();
        let c = config(&s).unwrap();
        assert_eq!((c.bot.as_str(), c.app.as_str()), ("xoxb-1", "xapp-1"));
        assert_eq!(c.users, vec!["U1A"]);
        // The two tokens aren't each other's, nor anything else.
        for (name, v) in [(SLACK_BOT_TOKEN, "xapp-1"), (SLACK_APP_TOKEN, "xoxb-1")] {
            let err = s
                .update(otter_protocol::host::SettingsUpdate {
                    secrets: [(name.to_owned(), Some(v.to_owned()))].into(),
                    ..Default::default()
                })
                .unwrap_err();
            assert!(err.contains(name), "{err}");
        }
        // Kept with the other secrets, never in the view.
        let view = serde_json::to_string(&s.view()).unwrap();
        assert!(!view.contains("xoxb-1") && !view.contains("xapp-1"));
        assert!(
            s.view()
                .secrets
                .iter()
                .any(|k| k.name == SLACK_BOT_TOKEN && k.set)
        );
    }
}
