//! Otter in Slack (D-200): the developer's direct messages to the Slack app
//! set on this host are said in Otter's conversation here (D-093), and
//! Otter's messages there go back to Slack while the developer last spoke
//! from it.
//!
//! When a task needs the developer, Slack is told, in a message of its own
//! with the buttons the app's decision card has (D-201); a reply in that
//! message's thread is said in the task's conversation (or answers its
//! question), and Otter's and the coding agent's messages there come back
//! into the thread.
//!
//! Slack's Socket Mode: otterd opens a WebSocket to Slack — outbound, like
//! its calls to model providers; nothing listens (D-003) — and acks each
//! event Slack sends down it. The Web API goes through curl (`api.rs`).
//!
//! ```text
//! state/slack.json   what was sent to Slack last: the last message looked
//!                    at in each conversation, the need last told for each
//!                    task, and the messages that told them
//! ```
//!
//! One connection per Slack app: Slack shares a connection's events among
//! every connection the app has open, so two hosts would each hear some of
//! them. Several hosts share one app through otterhub (`otter hub serve`,
//! D-206): it holds the app's one connection and gives each host what is
//! its own (`slack.deliver`); the host says Otter's messages itself, as
//! here, and opens no connection of its own.

mod needs;

use otter_slack::{api, socket};

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use otter_core::DecisionId;
use otter_core::TaskId;
use otter_core::conversation::{InteractionKind, Question};
use otter_core::task::{Message, MessageRole, RunState, TaskAction};
use otter_protocol::Event;
use otter_protocol::RpcError;
use otter_protocol::host::{SlackDeliver, SlackDelivered, SlackOwns, SlackView};
use otter_protocol::otter::OtterSend;
use otter_protocol::task::{TaskAct, TaskImage, TaskSend};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as Frame;

use crate::daemon::{Daemon, RpcResult};
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
/// What happens at once (a run's changes) is looked at once.
const SETTLE: Duration = Duration::from_millis(300);
/// The messages that told a need kept, at most: the newest.
const MAX_POSTS: usize = 200;
/// An answer being written is shown again at most this often (Slack's
/// `chat.update` is rate-limited, D-202).
const STREAM_EVERY: Duration = Duration::from_millis(1500);
/// A piece of the coding agent's said whole is shown at most this often
/// (D-203): a chatty agent mustn't run into Slack's limit.
const PIECE_EVERY: Duration = Duration::from_millis(400);
/// How long the sync waits for an answer's first words to be posted.
const STREAM_WAIT: Duration = Duration::from_secs(3);
/// What a quiet coding agent's thread says of it is shown again at most
/// this often (D-211).
const BUSY_EVERY: Duration = Duration::from_secs(10);
/// A task made from Slack within this long has what it said since its
/// request sent to its thread (D-207); one older, from then on.
const FRESH: chrono::TimeDelta = chrono::TimeDelta::minutes(2);
/// Settings' word on a host that takes Slack's events from otterhub (D-206).
const HUB: &str = "Slack's events come from otterhub (otter hub serve), which holds this app's one connection; Otter answers in Slack from here.";
/// Said under a need whose buttons Slack refused: told in its words alone.
const UNSHOWN: &str = "Slack didn't take this message's buttons: answer in the app, or reply in this thread to tell the task.";

/// The connection's state, for Settings.
#[derive(Default)]
pub struct Slack {
    status: std::sync::Mutex<Status>,
    restart: tokio::sync::Notify,
    /// Each listed member's direct messages with the app, once opened.
    channels: std::sync::Mutex<HashMap<String, String>>,
    /// Threads made outside the sync loop (`/otter new`, D-202), for it to
    /// keep: it alone writes `state/slack.json`.
    pending: std::sync::Mutex<Vec<Pending>>,
    /// The `/otter` commands done lately, by their trigger: one sent again
    /// (its ack was late) is answered, not done again (D-202).
    commands: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// What each form's selects chose, by its message and question, as
    /// they are chosen (D-202): Submit reads the message's state too.
    picks: std::sync::Mutex<HashMap<(String, String), String>>,
    /// Otter's answers being written, by the developer's message they
    /// answer (D-202); the coding agent's, by `agent:<task>` (D-203).
    streams: std::sync::Mutex<HashMap<String, Stream>>,
    /// The coding agent's last message per task, and when: its turn's
    /// words that come after it are dropped (D-203).
    settled: std::sync::Mutex<HashMap<String, (String, std::time::Instant)>>,
    /// The last press for each task, made outside the sync loop, for it to
    /// keep (D-207).
    pressed: std::sync::Mutex<HashMap<String, Pressed>>,
}

/// An answer of Otter's being written, shown in Slack as it is (D-202).
#[derive(Clone, Debug)]
struct Stream {
    /// Where it goes; `None`: not Slack's (the developer spoke elsewhere).
    to: Option<Origin>,
    /// The message it is written in, once posted.
    ts: Option<String>,
    /// That message is being posted, or shown again: the sync waits for
    /// it, so the answer is never overwritten by words before it.
    posting: bool,
    /// The answer itself was sent: nothing more is shown.
    over: bool,
    /// The words so far, and when Slack was last shown them.
    text: String,
    shown: Option<std::time::Instant>,
    /// What Slack shows is a piece said whole (D-203).
    whole: bool,
    /// When words for it last came: an answer being written, however long,
    /// is kept while they come.
    seen: std::time::Instant,
    /// When the coding agent last said anything for it, or it began: how
    /// long the agent has been quiet (D-211). Kept apart from `seen`, which
    /// a run going on keeps fresh.
    heard: std::time::Instant,
    /// What Slack shows of the coding agent at work while it is quiet
    /// (D-211), under its words or in their place; `None`: nothing.
    busy: Option<String>,
}

/// A thread for a task, made outside the sync loop.
#[derive(Clone, Debug, PartialEq)]
struct Pending {
    post: Post,
    /// Its conversation's last message when the thread began.
    after: Option<String>,
}

impl Slack {
    /// The need's (or task's) message whose thread `on` is, among those not
    /// kept yet.
    fn pending_post(&self, on: &Origin) -> Option<Post> {
        let thread = on.thread.as_deref()?;
        self.pending
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.post.ts == thread && p.post.channel == on.channel)
            .map(|p| p.post.clone())
    }
}

#[derive(Clone, Debug, Default)]
struct Status {
    /// `connecting`, `connected` or `failed`; `None`: not started.
    state: Option<&'static str>,
    detail: Option<String>,
    team: Option<String>,
    bot: Option<String>,
    bot_id: Option<String>,
}

impl Slack {
    /// A press on `task`'s message, at `on` (D-207): kept for the sync.
    fn press_on(&self, task: &str, on: &Origin) {
        let Some(ts) = on.thread.clone() else {
            return;
        };
        self.pressed.lock().unwrap().insert(
            task.to_owned(),
            Pressed {
                channel: on.channel.clone(),
                ts,
                at: chrono::Utc::now(),
            },
        );
    }

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
    /// The app token, for a connection of this host's own; `None`: Slack's
    /// events come from otterhub (D-206).
    app: Option<String>,
    users: Vec<String>,
    /// What this host's messages at the top of the conversation are
    /// labelled with: the name otterhub knows it by, while it serves it
    /// (D-207).
    label: Option<String>,
}

/// The config, or what is missing from it.
fn config(s: &HostSettings) -> Result<Config, String> {
    let (bot, app) = (s.secret(SLACK_BOT_TOKEN), s.secret(SLACK_APP_TOKEN));
    // otterhub holds the app's connection: its token isn't this host's.
    let app = match s.slack_hub() {
        true => None,
        false => Some(app),
    };
    let users = s.slack_users().to_vec();
    let mut missing = Vec::new();
    if bot.is_none() {
        missing.push("the bot token (xoxb-…)");
    }
    if app.as_ref().is_some_and(Option::is_none) {
        missing.push("the app token (xapp-…)");
    }
    if users.is_empty() {
        missing.push("your Slack member ID");
    }
    match (bot, missing.is_empty()) {
        (Some(bot), true) => Ok(Config {
            bot,
            app: app.flatten(),
            users,
            label: s
                .slack_hub()
                .then(|| s.slack_name().map(String::from))
                .flatten(),
        }),
        _ => Err(format!("Add {}.", missing.join(", "))),
    }
}

pub use otter_slack::members;

/// What was sent to Slack, kept across restarts (`state/slack.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// The last of Otter's conversation's messages looked at for Slack:
    /// sent, or not Slack's. `None`: from the start.
    #[serde(default)]
    after: Option<String>,
    /// The same for each task with a thread in Slack (D-201).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tasks: BTreeMap<String, Option<String>>,
    /// The need last seen on each task, by its key. `None` until the first
    /// look, which tells nothing: what waited before is not news.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    needs: Option<BTreeMap<String, String>>,
    /// The messages that told a need, newest last: each one's thread is its
    /// task's. A task's own message too (`started`, D-202, D-207).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    posts: Vec<Post>,
    /// Where the developer last pressed a button for each task, and when
    /// (D-207): what the task says next goes to that message's thread.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pressed: BTreeMap<String, Pressed>,
}

/// A message that told the developer a task needed them, or a task's own
/// message (`key` `started`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Post {
    task: String,
    /// The need it told (`needs::key`), or `started`.
    key: String,
    channel: String,
    ts: String,
    /// What it said, kept when its buttons go.
    text: String,
    /// Its buttons are still there: the need isn't over.
    #[serde(default)]
    open: bool,
    /// The task's messages said in this thread though not written in Slack
    /// (D-207): the request Otter on the host made it with, from the
    /// developer's message here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    said: Vec<String>,
    /// A task's own message: what it shows of the task now
    /// (`needs::card_key`), so it is shown again once that changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shows: Option<String>,
}

/// A press on one of a task's messages (D-207).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Pressed {
    channel: String,
    /// The message pressed on: its thread is where the task says what came
    /// of it.
    ts: String,
    at: otter_core::Timestamp,
}

impl Pressed {
    fn origin(&self) -> Origin {
        Origin {
            channel: self.channel.clone(),
            thread: Some(self.ts.clone()),
        }
    }
}

/// Where the developer spoke to a task from Slack (D-207), besides what
/// their messages' own `correlation_id`s say.
#[derive(Clone, Debug, Default)]
struct Heard {
    /// The task's messages said in a thread of its: by message id.
    said: HashMap<String, Origin>,
    /// The last press on one of its messages.
    pressed: Option<Pressed>,
}

impl Heard {
    /// Where the developer's message `m` was said in Slack, if it was.
    fn origin(&self, m: &Message) -> Option<Origin> {
        Origin::of(m.correlation_id.as_deref()).or_else(|| self.said.get(m.id.as_str()).cloned())
    }
}

impl State {
    fn load(path: &Path) -> Option<State> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn save(&self, path: &Path) {
        let bytes = serde_json::to_vec(self).expect("serializes");
        if let Err(e) = crate::settings::write_private(path, &bytes) {
            tracing::warn!("slack: saving {}: {e:#}", path.display());
        }
    }

    /// The need's message whose thread `on` is.
    fn post_of(&self, on: &Origin) -> Option<Post> {
        let thread = on.thread.as_deref()?;
        self.posts
            .iter()
            .find(|p| p.ts == thread && p.channel == on.channel)
            .cloned()
    }

    /// Where the developer spoke to `task` from Slack, besides their
    /// messages' own `correlation_id`s; `pressed`: presses not kept yet.
    fn heard(&self, task: &str, pressed: Option<&Pressed>) -> Heard {
        let mut heard = Heard::default();
        for p in self.posts.iter().filter(|p| p.task == task) {
            let there = Origin {
                channel: p.channel.clone(),
                thread: Some(p.ts.clone()),
            };
            for id in &p.said {
                heard.said.insert(id.clone(), there.clone());
            }
        }
        heard.pressed = [self.pressed.get(task), pressed]
            .into_iter()
            .flatten()
            .max_by_key(|p| p.at)
            .cloned();
        heard
    }
}

/// The tasks as Slack needs them, read at once under the store's lock.
#[derive(Default)]
struct Look {
    /// Every task's title, by id: what a message names it by.
    titles: HashMap<String, String>,
    /// Every task's need now, by id: its key and what to tell; `None` when
    /// it needs nothing.
    needs: BTreeMap<String, Option<(String, needs::Told)>>,
    /// Each task's last message now, by id.
    last: HashMap<String, Option<String>>,
    /// Open posts whose need is over: their index, and what came of it.
    over: Vec<(usize, String)>,
    /// Tasks' own messages to show again (D-207): their index, what they
    /// show now, and how.
    cards: Vec<(usize, String, needs::Told)>,
    /// The conversations of the tasks with a thread, where they have
    /// something Slack hasn't looked at.
    messages: BTreeMap<String, Vec<Message>>,
    /// The tasks whose coding agent is between turns: no run going (D-203).
    quiet: std::collections::HashSet<String>,
    /// The tasks whose coding agent is at work and waits on no one, with
    /// the thread its words go to, and what it did last (D-211).
    working: HashMap<String, (Origin, Option<String>)>,
    /// When it was read: words that came after are a turn it didn't see.
    at: Option<std::time::Instant>,
}

impl Daemon {
    /// Settings' view of Slack: what is missing at once, else how the
    /// connection is.
    pub(crate) fn slack_view(&self) -> SlackView {
        let s = self.settings.read().unwrap();
        let users = s.slack_users().to_vec();
        let notify = Some(s.slack_notify());
        let hub = s.slack_hub().then_some(true);
        let name = s.slack_name().map(String::from);
        if let Err(missing) = config(&s) {
            return SlackView {
                users,
                state: "off".into(),
                detail: Some(missing),
                notify,
                hub,
                name,
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
            bot_id: st.bot_id,
            notify,
            hub,
            name,
        }
    }

    fn slack_status(&self, status: Status) {
        *self.slack.status.lock().unwrap() = status;
    }

    /// What Slack sent otterhub for this host (D-206), taken as a connection
    /// of this host's own takes it: a message said where it belongs, a press
    /// done, `/otter` answered — its answer for otterhub's ack. Only while
    /// Slack is set up here to take its events from otterhub.
    pub(crate) async fn slack_deliver(
        self: &Arc<Self>,
        p: SlackDeliver,
    ) -> RpcResult<SlackDelivered> {
        let c = {
            let s = self.settings.read().unwrap();
            if !s.slack_hub() {
                return Err(RpcError::conflict(
                    "this host doesn't take Slack's events from otterhub (its Settings › Slack has a connection of its own)",
                ));
            }
            config(&s).map_err(|missing| {
                RpcError::invalid(format!("Slack isn't set up on this host: {missing}"))
            })?
        };
        let env = self.environments.base().clone();
        let ack = match socket::envelope(&p.envelope.to_string()) {
            Envelope::Event { event, .. } => {
                self.slack_heard(&c, &env, &event).await;
                None
            }
            Envelope::Interactive { payload, .. } => {
                self.slack_pressed(&c, &env, &payload);
                None
            }
            Envelope::Command { payload, .. } => Some(self.slack_command(&c, &env, &payload).await),
            _ => None,
        };
        Ok(SlackDelivered { ack })
    }

    /// Whether the message at `ts` in `channel` is this host's (D-206): one
    /// that told a need or began a task's thread here, or one posted from
    /// here lately — so a reply in its thread, or a press on it, is too.
    pub(crate) fn slack_owns(&self, p: &SlackOwns) -> bool {
        let on = Origin {
            channel: p.channel.clone(),
            thread: Some(p.ts.clone()),
        };
        let path = self.paths.state_dir.join("slack.json");
        State::load(&path).and_then(|s| s.post_of(&on)).is_some()
            || self.slack.pending_post(&on).is_some()
            || api::posted_lately(&p.channel, &p.ts)
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
                            let next = match c.app {
                                Some(_) => {
                                    "Check the Slack app's two tokens and Socket Mode in Settings › Slack; Otter tries again meanwhile."
                                }
                                None => {
                                    "Check the Slack app's bot token otterhub set here (`otter hub serve`); Otter tries again meanwhile."
                                }
                            };
                            self.note_problem(
                                "slack",
                                format!("Otter isn't connected to Slack: {why}."),
                                next.into(),
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

    /// One connection, until Slack closes it (`Ok`) or it fails. Taking
    /// Slack's events from otterhub (D-206), none is opened: what Slack says
    /// comes through `slack.deliver`, and the rest is the same.
    async fn slack_session(self: &Arc<Self>, c: &Config) -> anyhow::Result<()> {
        *LABEL.lock().unwrap() = c.label.clone();
        // A post the last connection was cut off in the middle of never
        // came back: nothing is on its way now (D-211).
        for s in self.slack.streams.lock().unwrap().values_mut() {
            s.posting = false;
        }
        let env = self.environments.base().clone();
        let me = api::call(&env, &c.bot, "auth.test", &json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("{e} (the bot token)"))?;
        let socket = match &c.app {
            Some(app) => {
                let open = api::call(&env, app, "apps.connections.open", &json!({}))
                    .await
                    .map_err(|e| anyhow::anyhow!("{e} (the app token)"))?;
                // Its address carries a ticket: never logged.
                let url = open["url"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Slack gave no address to connect to"))?;
                let (ws, _) = tokio_tungstenite::connect_async(url)
                    .await
                    .map_err(|e| anyhow::anyhow!("connecting to Slack: {e}"))?;
                Some(ws.split())
            }
            None => None,
        };
        let (_, mut events) = self.events.subscribe();
        let mut writing = self.events.subscribe_transient();
        // Otter's answers as they are written (D-202).
        let written = async {
            loop {
                match writing.recv().await {
                    Ok(r) => self.slack_writing(c, &env, &r.event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        };
        // Where Slack starts, taken now: what happens from here on is news.
        self.slack_begin().await;
        let wake = tokio::sync::Notify::new();
        // What goes to Slack goes apart from reading, so an ack never waits
        // on a post.
        // Also looked at now and then without news: a coding agent at
        // work that says nothing is shown so (D-211).
        let tick = (agent_quiet() / 4).clamp(Duration::from_millis(100), Duration::from_secs(30));
        let sync = async {
            loop {
                let _ = tokio::time::timeout(tick, wake.notified()).await;
                tokio::time::sleep(SETTLE).await;
                match self.slack_sync(c, &env).await {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::warn!("slack: sending to Slack: {e}");
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        wake.notify_one();
                    }
                }
            }
        };
        let read = async {
            let Some((mut sink, mut stream)) = socket else {
                // The host can't tell whether otterhub is there: it says
                // only that Slack's events are its to give.
                tracing::info!("slack: Slack's events come from otterhub");
                self.clear_problem("slack");
                self.slack_status(Status {
                    state: Some("hub"),
                    detail: Some(HUB.into()),
                    team: me["team"].as_str().map(String::from),
                    bot: me["user"].as_str().map(String::from),
                    bot_id: me["bot_id"].as_str().map(String::from),
                });
                wake.notify_one();
                loop {
                    match worth_a_look(events.recv().await) {
                        Some(true) => wake.notify_one(),
                        Some(false) => {}
                        None => return Ok(()),
                    }
                }
            };
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
                                    bot_id: me["bot_id"].as_str().map(String::from),
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
                            Envelope::Interactive { envelope_id, payload } => {
                                let ack = json!({ "envelope_id": envelope_id }).to_string();
                                sink.send(Frame::text(ack)).await?;
                                self.slack_pressed(c, &env, &payload);
                            }
                            Envelope::Command { envelope_id, payload } => {
                                let answer = self.slack_command(c, &env, &payload).await;
                                let ack = json!({ "envelope_id": envelope_id, "payload": answer });
                                sink.send(Frame::text(ack.to_string())).await?;
                            }
                            Envelope::Other { envelope_id } => {
                                let ack = json!({ "envelope_id": envelope_id }).to_string();
                                sink.send(Frame::text(ack)).await?;
                            }
                            Envelope::Unknown => {}
                        }
                    }
                    ev = events.recv() => match worth_a_look(ev) {
                        Some(true) => wake.notify_one(),
                        Some(false) => {}
                        None => return Ok(()),
                    },
                }
            }
        };
        tokio::select! {
            r = read => r,
            _ = sync => Ok(()),
            () = written => Ok(()),
        }
    }

    /// An event from Slack: the developer's direct message, said where it
    /// belongs — under a need's message, in its task's conversation (or as
    /// the answer to that question, D-201); else in Otter's on the host —
    /// and marked seen (👀) once it is. An answer, like a press, is done
    /// apart from reading: it goes on to the coding agent, and a restart
    /// mustn't cut it short. So is a message with pictures, which are
    /// fetched first (D-202).
    async fn slack_heard(self: &Arc<Self>, c: &Config, env: &EnvMap, event: &Value) {
        let m = match socket::inbound(event, &c.users) {
            Ok(m) => m,
            Err(socket::Skip::NotListed) => {
                tracing::info!("slack: a message from a Slack member Settings doesn't list");
                return;
            }
            Err(_) => return,
        };
        let (env, bot) = (env.clone(), c.bot.clone());
        if m.files.is_empty() {
            self.slack_say(&env, &bot, m, vec![]).await;
            return;
        }
        let daemon = self.clone();
        tokio::spawn(async move {
            let (images, left) = pictures(&env, &bot, &m.files).await;
            if left > 0 {
                let line = match left {
                    1 => "A file wasn't passed on: Otter takes pictures (png, jpeg or webp, up to 700 KB, four at most).".to_owned(),
                    n => format!("{n} files weren't passed on: Otter takes pictures (png, jpeg or webp, up to 700 KB, four at most)."),
                };
                if let Err(e) = post(&env, &bot, &m.to, &line, &HashMap::new()).await {
                    tracing::warn!("slack: {e}");
                }
            }
            if m.text.is_empty() && images.is_empty() {
                return;
            }
            daemon.slack_say(&env, &bot, m, images).await;
        });
    }

    /// Say the developer's message (and its pictures) where it belongs, and
    /// mark it.
    async fn slack_say(
        self: &Arc<Self>,
        env: &EnvMap,
        bot: &str,
        m: socket::Inbound,
        images: Vec<TaskImage>,
    ) {
        let (env, bot) = (env.clone(), bot.to_owned());
        let path = self.paths.state_dir.join("slack.json");
        let under = State::load(&path)
            .and_then(|s| s.post_of(&m.to))
            .or_else(|| self.slack.pending_post(&m.to));
        let Some(under) = under else {
            let sent = self
                .otter_send(OtterSend {
                    command_id: m.command_id(),
                    text: m.text.clone(),
                    images,
                })
                .await
                .map(|_| ());
            tokio::spawn(async move { mark(&env, &bot, &m, sent).await });
            return;
        };
        let question = self
            .tasks
            .lock()
            .await
            .get(&under.task)
            .and_then(needs::open_question);
        // Words alone answer a question; with pictures they are said.
        let answer = answers(&under, question).filter(|_| images.is_empty() && !m.text.is_empty());
        match answer {
            Some(decision_id) => {
                let act = TaskAct {
                    command_id: m.command_id(),
                    task: under.task,
                    action: TaskAction::Decide {
                        decision_id,
                        approve: true,
                        answer: Some(m.text.clone()),
                        answers: None,
                    },
                };
                let daemon = self.clone();
                tokio::spawn(async move {
                    let sent = daemon.task_act(act).await.map(|_| ());
                    mark(&env, &bot, &m, sent).await;
                });
            }
            None => {
                let sent = self
                    .task_send(TaskSend {
                        command_id: m.command_id(),
                        task: under.task,
                        text: m.text.clone(),
                        images,
                        command: None,
                    })
                    .await
                    .map(|_| ());
                tokio::spawn(async move { mark(&env, &bot, &m, sent).await });
            }
        }
    }

    /// A button pressed on a need's message (D-201): done as the app's
    /// decision card does it, once per press, only for a listed member —
    /// anyone who sees the message can press it. What came of it shows when
    /// the task changes; a refusal is said in the thread.
    fn slack_pressed(self: &Arc<Self>, c: &Config, env: &EnvMap, payload: &Value) {
        let Some(p) = socket::press(payload) else {
            return;
        };
        if !c.users.contains(&p.user) {
            tracing::info!("slack: a button pressed by a Slack member Settings doesn't list");
            return;
        }
        // A form's select, as it is chosen: kept for its Submit (D-202).
        if p.action == "pick" {
            if let (Some(ts), Some(question)) = (p.on.thread.clone(), p.block.strip_prefix("q:")) {
                self.slack
                    .picks
                    .lock()
                    .unwrap()
                    .insert((ts, question.to_owned()), p.picked.join(", "));
            }
            return;
        }
        let v: Value = serde_json::from_str(&p.value).unwrap_or_default();
        let (Some(task), decision) = (v["task"].as_str(), v["decision"].as_str()) else {
            return;
        };
        // What the task says of it is said under the message pressed on.
        self.slack.press_on(task, &p.on);
        if p.action == "delete" {
            let (daemon, env, bot, task) =
                (self.clone(), env.clone(), c.bot.clone(), task.to_owned());
            tokio::spawn(async move {
                match daemon.task_delete(&task).await {
                    // Gone already (pressed twice): what was asked.
                    Ok(()) => {}
                    Err(e) if e.code == otter_protocol::ErrorCode::NotFound => {}
                    Err(e) => {
                        let line = format!("That didn't go through: {}", e.message);
                        if let Err(e) = post(&env, &bot, &p.on, &line, &HashMap::new()).await {
                            tracing::warn!("slack: {e}");
                        }
                    }
                }
            });
            return;
        }
        if p.action == "form" {
            let Some(decision) = decision else {
                return;
            };
            let (daemon, env, bot) = (self.clone(), env.clone(), c.bot.clone());
            let (task, decision) = (task.to_owned(), DecisionId::from(decision));
            tokio::spawn(async move {
                let line = match daemon.slack_submit(&p, &task, &decision).await {
                    Ok(()) => return,
                    Err(line) => line,
                };
                if let Err(e) = post(&env, &bot, &p.on, &line, &HashMap::new()).await {
                    tracing::warn!("slack: {e}");
                }
            });
            return;
        }
        let decide = |approve: bool, answer: Option<String>| {
            decision.map(|d| TaskAction::Decide {
                decision_id: d.into(),
                approve,
                answer,
                answers: None,
            })
        };
        let action = match p.action.as_str() {
            "approve" => decide(true, None),
            "deny" => decide(false, None),
            "retry" => Some(TaskAction::Retry),
            "stop" => Some(TaskAction::Cancel),
            a if a.starts_with("answer:") => decide(true, v["answer"].as_str().map(String::from)),
            _ => None,
        };
        let Some(action) = action else {
            return;
        };
        let stop = matches!(action, TaskAction::Cancel);
        let act = TaskAct {
            command_id: format!("slack:act:{}", p.at),
            task: task.to_owned(),
            action,
        };
        let (daemon, env, bot) = (self.clone(), env.clone(), c.bot.clone());
        tokio::spawn(async move {
            if let Err(e) = daemon.clone().task_act(act.clone()).await {
                // Over already (stopped from the app, or pressed late):
                // what was asked (D-207).
                let over = |f: otter_core::task::Task| {
                    use otter_core::task::TaskStatus;
                    matches!(f.status, TaskStatus::Cancelled | TaskStatus::Done)
                };
                if stop && daemon.task_get(&act.task).await.is_ok_and(over) {
                    return;
                }
                let line = format!("That didn't go through: {}", e.message);
                if let Err(e) = post(&env, &bot, &p.on, &line, &HashMap::new()).await {
                    tracing::warn!("slack: {e}");
                }
            }
        });
    }

    /// `/otter` (D-202): answered in its ack, seen only by who typed it.
    /// `status` says what needs the developer and what is at work; `new`
    /// makes a task of its words and starts it, in a thread of its own in
    /// their direct messages; anything else, what `/otter` does.
    async fn slack_command(self: &Arc<Self>, c: &Config, env: &EnvMap, payload: &Value) -> Value {
        let answer = |text: String| {
            json!({
                "response_type": "ephemeral",
                "text": text.chars().take(280).collect::<String>(),
                "blocks": [{ "type": "markdown", "text": text }],
            })
        };
        let Some(cmd) = socket::command(payload) else {
            return answer("Otter only knows `/otter`.".into());
        };
        if !c.users.contains(&cmd.user) {
            tracing::info!("slack: /otter from a Slack member Settings doesn't list");
            return answer("This Otter answers only the developer it is set up for.".into());
        }
        match cmd.verb {
            socket::Verb::Status => {
                // Answered within Slack's three seconds, or not at all.
                match tokio::time::timeout(Duration::from_secs(1), self.tasks.lock()).await {
                    Ok(store) => answer(needs::status(store.iter())),
                    Err(_) => answer("Otter is busy; ask again in a moment.".into()),
                }
            }
            socket::Verb::Help => answer(needs::HELP.into()),
            socket::Verb::New(request) => {
                let title = needs::title_of(&request);
                let (daemon, env, bot) = (self.clone(), env.clone(), c.bot.clone());
                let reply = format!(
                    "Starting **{title}**: its thread is in your direct messages with Otter."
                );
                {
                    let mut done = self.slack.commands.lock().unwrap();
                    if !cmd.trigger.is_empty() {
                        if done.contains(&cmd.trigger) {
                            return answer(reply);
                        }
                        done.push_back(cmd.trigger.clone());
                        while done.len() > 64 {
                            done.pop_front();
                        }
                    }
                }
                tokio::spawn(async move {
                    if let Err(e) = daemon
                        .slack_new(&env, &bot, &cmd.user, &title, request)
                        .await
                    {
                        tracing::warn!("slack: /otter new: {e}");
                        if let Some(url) = &cmd.respond {
                            let line = format!("Otter didn't start **{title}**: {e}");
                            if let Err(e) = api::respond(&env, url, &line).await {
                                tracing::warn!("slack: {e}");
                            }
                        }
                    }
                });
                answer(reply)
            }
        }
    }

    /// Make a task of `request` and start it, from Slack (D-202): first the
    /// message whose thread is the task's, then the task, its request said
    /// as if written in that thread (`command_id` `slack:<dm>:<ts>:<ts>`, so
    /// D-200's rule sends what follows it there).
    async fn slack_new(
        self: &Arc<Self>,
        env: &EnvMap,
        bot: &str,
        user: &str,
        title: &str,
        request: String,
    ) -> Result<(), CallError> {
        let channel = self.slack_dm(env, bot, user).await?;
        let text = format!("**{title}**\n\n_Starting. Reply in this thread to talk to it._");
        let started = api::call(
            env,
            bot,
            "chat.postMessage",
            &json!({
                "channel": channel,
                "text": title,
                "blocks": shown(&text, &Origin { channel: channel.clone(), thread: None }),
                "unfurl_links": false,
            }),
        )
        .await?;
        let ts = started["ts"].as_str().unwrap_or_default().to_owned();
        let to = Origin {
            channel: channel.clone(),
            thread: Some(ts.clone()),
        };
        let created = self
            .task_create(otter_protocol::task::TaskCreate {
                command_id: format!("slack:{channel}:{ts}:{ts}"),
                title: title.to_owned(),
                request,
                workspace: None,
                answer_only: false,
            })
            .await;
        let task = match created {
            Ok(task) => task,
            Err(e) => {
                let line = format!("Otter didn't make that task: {}", e.message);
                return post(env, bot, &to, &line, &HashMap::new()).await;
            }
        };
        self.slack.pending.lock().unwrap().push(Pending {
            post: Post {
                task: task.id.to_string(),
                key: "started".into(),
                channel,
                ts: ts.clone(),
                text,
                open: false,
                said: vec![],
                shows: None,
            },
            after: task.messages.last().map(|m| m.id.to_string()),
        });
        let start = TaskAct {
            command_id: format!("slack:start:{ts}"),
            task: task.id.to_string(),
            action: TaskAction::Start,
        };
        if let Err(e) = self.clone().task_act(start).await {
            let line = format!("Otter made the task but didn't start it: {}", e.message);
            post(env, bot, &to, &line, &HashMap::new()).await?;
        }
        Ok(())
    }

    /// Otter's answer being written (D-202): its first words posted where it
    /// will go, then shown again as more come, at most every 1.5 s. The
    /// answer itself takes that message's place when the sync sends it.
    /// The coding agent's words too (D-203): one message per task, showing
    /// what it says as it says it, each piece whole once said, until the
    /// message its turn ends with takes its place — as the app shows them
    /// (D-069).
    async fn slack_writing(&self, c: &Config, env: &EnvMap, event: &Event) {
        enum From<'a> {
            /// The developer's message on the host, by id.
            Host(&'a str),
            /// A task's message by id; `None`: its latest from the developer.
            Task(&'a TaskId, Option<&'a str>),
        }
        let (key, text, done, from) = match event {
            Event::OtterStream {
                stream_id,
                text,
                done: false,
            } => {
                let Some(id) = stream_id.strip_prefix("reply-") else {
                    return;
                };
                (id.to_owned(), text, false, From::Host(id))
            }
            Event::TaskStream {
                task_id,
                stream_id,
                role: MessageRole::Controller,
                text,
                done: false,
            } => {
                let Some(id) = stream_id.strip_prefix("reply-") else {
                    return;
                };
                (id.to_owned(), text, false, From::Task(task_id, Some(id)))
            }
            Event::TaskStream {
                task_id,
                role: MessageRole::Agent,
                text,
                done,
                ..
            } => (
                format!("agent:{task_id}"),
                text,
                *done,
                From::Task(task_id, None),
            ),
            _ => return,
        };
        // Words of a turn whose message was sent already, come late.
        if let Some((said, at)) = self.slack.settled.lock().unwrap().get(&key)
            && at.elapsed() < Duration::from_secs(30)
            && said.starts_with(text.as_str())
        {
            return;
        }
        let answered = key.as_str();
        let known = self.slack.streams.lock().unwrap().contains_key(answered);
        if !known {
            // Where the developer's message came from, once.
            let to = match from {
                From::Host(id) => self
                    .chat
                    .lock()
                    .await
                    .get()
                    .messages
                    .iter()
                    .find(|m| m.id.as_str() == id)
                    .and_then(|m| Origin::of(m.correlation_id.as_deref())),
                From::Task(task, id) => {
                    let heard = self.slack_heard_by(task.as_str());
                    let store = self.tasks.lock().await;
                    let t = store.get(task.as_str());
                    let m = t
                        .zip(id)
                        .and_then(|(t, id)| t.messages.iter().find(|m| m.id.as_str() == id));
                    match (t, id) {
                        (Some(_), Some(_)) => {
                            let to = m.and_then(|m| heard.origin(m));
                            // A task made from Slack a moment ago may not
                            // have its own message yet (D-207): looked for
                            // again with the next words.
                            if to.is_none() && m.is_some_and(just_made) {
                                return;
                            }
                            to
                        }
                        // The agent's: where the developer is now.
                        (Some(t), None) => spoke(&t.messages, chrono::Utc::now(), &heard),
                        (None, _) => None,
                    }
                }
            };
            let mut streams = self.slack.streams.lock().unwrap();
            // (The coding agent's live as long as its run: `slack_turns_ended`.)
            streams.retain(|k, s| {
                k.starts_with("agent:") || s.seen.elapsed() < Duration::from_secs(600)
            });
            streams.entry(answered.to_owned()).or_insert(Stream {
                to,
                ts: None,
                posting: false,
                over: false,
                text: String::new(),
                shown: None,
                whole: false,
                seen: std::time::Instant::now(),
                heard: std::time::Instant::now(),
                busy: None,
            });
        }
        // What to do now, decided under the lock; done without it.
        enum Next {
            Post(Origin),
            Update(Origin, String),
        }
        let next = {
            let mut streams = self.slack.streams.lock().unwrap();
            let Some(s) = streams.get_mut(answered) else {
                return;
            };
            s.text = text.clone();
            s.seen = std::time::Instant::now();
            // Heard from: what it was shown at goes with these words, at
            // once (D-211).
            s.heard = s.seen;
            let was_busy = !s.posting && s.busy.take().is_some();
            match (&s.to, &s.ts) {
                _ if s.over || s.posting => None,
                (None, _) => None,
                (Some(to), None) => {
                    s.posting = true;
                    s.whole = done;
                    Some(Next::Post(to.clone()))
                }
                (Some(to), Some(ts))
                    if was_busy
                        || s.shown.is_none_or(|t| {
                            t.elapsed() >= STREAM_EVERY || (done && t.elapsed() >= PIECE_EVERY)
                        }) =>
                {
                    s.shown = Some(std::time::Instant::now());
                    s.whole = done;
                    s.posting = true;
                    Some(Next::Update(to.clone(), ts.clone()))
                }
                _ => None,
            }
        };
        // Nothing written yet is Otter starting to write (D-196); a piece
        // said whole has no "…".
        let words = match socket::pieces(text, MAX_PIECE - 2).first() {
            Some(said) if done => said.clone(),
            Some(so_far) => format!("{so_far} …"),
            None => "_Otter is writing…_".to_owned(),
        };
        match next {
            None => {}
            Some(Next::Post(to)) => {
                let mut body = json!({
                    "channel": to.channel,
                    "text": words.chars().take(280).collect::<String>(),
                    "blocks": shown(&words, &to),
                    "unfurl_links": false,
                    "unfurl_media": false,
                });
                if let Some(t) = &to.thread {
                    body["thread_ts"] = json!(t);
                }
                let posted = api::call(env, &c.bot, "chat.postMessage", &body).await;
                let mut streams = self.slack.streams.lock().unwrap();
                if let Some(s) = streams.get_mut(answered) {
                    s.posting = false;
                    match posted {
                        Ok(v) => {
                            s.ts = v["ts"].as_str().map(String::from);
                            s.shown = Some(std::time::Instant::now());
                        }
                        // Shown once it is sent, then.
                        Err(e) => {
                            tracing::info!("slack: an answer being written wasn't shown: {e}");
                            s.over = true;
                        }
                    }
                }
            }
            Some(Next::Update(to, ts)) => {
                let body = json!({
                    "channel": to.channel,
                    "ts": ts,
                    "text": words.chars().take(280).collect::<String>(),
                    "blocks": shown(&words, &to),
                });
                if let Err(e) = api::call(env, &c.bot, "chat.update", &body).await {
                    tracing::info!("slack: an answer being written wasn't shown again: {e}");
                }
                if let Some(s) = self.slack.streams.lock().unwrap().get_mut(answered) {
                    s.posting = false;
                }
            }
        }
    }

    /// Send one of Otter's (or the coding agent's) messages to `to`, where
    /// it goes (`None`: nowhere in Slack). Words shown while one of Otter's
    /// answers was written are replaced by it where they are (D-202) —
    /// also when the answer itself goes elsewhere now, or nowhere — so none
    /// is left half written; and nothing written for it is shown after.
    async fn slack_answer(
        &self,
        env: &EnvMap,
        bot: &str,
        task: Option<&str>,
        to: Option<&Origin>,
        m: &Message,
        titles: &HashMap<String, String>,
    ) -> Result<(), CallError> {
        let agent = m.role == MessageRole::Agent;
        let answered = match (m.role, task) {
            (MessageRole::Controller, _) => m.correlation_id.clone(),
            // The coding agent's: its turn's words, shown in its task (D-203).
            (MessageRole::Agent, Some(task)) => Some(format!("agent:{task}")),
            _ => None,
        };
        let Some(answered) = answered else {
            return match to {
                Some(to) => post(env, bot, to, &m.text, titles).await,
                None => Ok(()),
            };
        };
        // Words shown for it may still be on their way: wait for them.
        let waited = std::time::Instant::now();
        let written = loop {
            {
                let mut streams = self.slack.streams.lock().unwrap();
                // (The coding agent's live as long as its run: `slack_turns_ended`.)
                streams.retain(|k, s| {
                    k.starts_with("agent:") || s.seen.elapsed() < Duration::from_secs(600)
                });
                match streams.get_mut(&answered) {
                    Some(s) if s.posting && waited.elapsed() < STREAM_WAIT => {}
                    Some(s) => {
                        s.over = true;
                        let written = s.to.clone().zip(s.ts.clone());
                        // The agent's next turn is shown afresh.
                        if agent {
                            streams.remove(&answered);
                        }
                        break written;
                    }
                    // The agent's words come per turn; nothing to stop.
                    None if agent => break None,
                    None => {
                        // Words that come late are for an answer already sent.
                        streams.insert(
                            answered.clone(),
                            Stream {
                                to: None,
                                ts: None,
                                posting: false,
                                over: true,
                                text: String::new(),
                                shown: None,
                                whole: false,
                                seen: std::time::Instant::now(),
                                heard: std::time::Instant::now(),
                                busy: None,
                            },
                        );
                        break None;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        if agent {
            // Its turn's words that come late are dropped.
            self.slack.settled.lock().unwrap().insert(
                answered.clone(),
                (m.text.clone(), std::time::Instant::now()),
            );
        }
        let Some((there, ts)) = written else {
            return match to {
                Some(to) => post(env, bot, to, &m.text, titles).await,
                None => Ok(()),
            };
        };
        let text = needs::titled(&otter_core::redact::redact(&m.text), titles);
        let pieces = socket::pieces(&text, MAX_PIECE);
        let first = pieces.first().cloned().unwrap_or_default();
        let body = json!({
            "channel": there.channel,
            "ts": ts,
            "text": first.chars().take(280).collect::<String>(),
            "blocks": shown(&first, &there),
        });
        api::call(env, bot, "chat.update", &body).await?;
        for piece in pieces.iter().skip(1) {
            post(env, bot, &there, piece, titles).await?;
        }
        match to {
            Some(to) if *to != there => post(env, bot, to, &m.text, titles).await,
            _ => Ok(()),
        }
    }

    /// The first time Slack connects (or after its file was lost): what is
    /// there already — Otter's messages, the tasks' needs — isn't sent.
    async fn slack_begin(&self) {
        let path = self.paths.state_dir.join("slack.json");
        let mut state = match State::load(&path) {
            Some(state) if state.needs.is_some() => return,
            Some(state) => state,
            None => State {
                after: self
                    .chat
                    .lock()
                    .await
                    .get()
                    .messages
                    .last()
                    .map(|m| m.id.to_string()),
                ..Default::default()
            },
        };
        let look = self.slack_look(&state).await;
        state.needs = Some(baseline(&look));
        state.save(&path);
    }

    /// The questions of `decision`, when it was asked as a form (D-202):
    /// several questions, or one whose choices may be several. One with a
    /// single choice is asked with buttons, and one with none takes a reply.
    pub(crate) fn form_of(&self, task: &str, decision: &DecisionId) -> Option<Vec<Question>> {
        let questions = self
            .conversations
            .list(Some(task))
            .into_iter()
            .flat_map(|c| c.interactions)
            .find(|i| &i.id == decision)
            .and_then(|i| match i.kind {
                InteractionKind::Questions { questions } => Some(questions),
                _ => None,
            })?;
        (questions.len() > 1 || questions.iter().any(|q| q.multi_select)).then_some(questions)
    }

    /// A form's Submit (D-202): every question's answer, from what its
    /// selects chose as they were chosen and what the message holds now,
    /// done as the app's form does it. What isn't answered yet is said
    /// instead.
    async fn slack_submit(
        self: &Arc<Self>,
        p: &socket::Press,
        task: &str,
        decision: &DecisionId,
    ) -> Result<(), String> {
        let Some(questions) = self.form_of(task, decision) else {
            return Err("That form isn't asked any more.".into());
        };
        let ts = p.on.thread.clone().unwrap_or_default();
        let mut answers: std::collections::BTreeMap<String, String> = {
            let picks = self.slack.picks.lock().unwrap();
            questions
                .iter()
                .filter_map(|q| {
                    let a = picks.get(&(ts.clone(), q.id.clone()))?;
                    (!a.is_empty()).then(|| (q.id.clone(), a.clone()))
                })
                .collect()
        };
        answers.extend(socket::form_answers(&p.state));
        answers.retain(|q, _| questions.iter().any(|x| &x.id == q));
        let missing: Vec<&str> = questions
            .iter()
            .filter(|q| !answers.contains_key(&q.id))
            .map(|q| q.header.as_deref().unwrap_or(q.prompt.as_str()))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "Answer every question, then Submit. Still to answer: {}.",
                missing.join("; ")
            ));
        }
        let act = TaskAct {
            command_id: format!("slack:act:{}", p.at),
            task: task.to_owned(),
            action: TaskAction::Decide {
                decision_id: decision.clone(),
                approve: true,
                answer: None,
                answers: Some(answers),
            },
        };
        self.clone()
            .task_act(act)
            .await
            .map(|_| {
                self.slack
                    .picks
                    .lock()
                    .unwrap()
                    .retain(|(m, _), _| m != &ts);
            })
            .map_err(|e| format!("That didn't go through: {}", e.message))
    }

    /// Bring Slack up to date: Otter's messages on the host, each task's
    /// need, and the messages in the tasks' threads.
    async fn slack_sync(&self, c: &Config, env: &EnvMap) -> Result<(), CallError> {
        let path = self.paths.state_dir.join("slack.json");
        let chat = self.chat.lock().await.get().clone();
        // The first time (or a file that can't be read): what is there
        // already isn't sent.
        let mut state = State::load(&path).unwrap_or_else(|| {
            let state = State {
                after: chat.messages.last().map(|m| m.id.to_string()),
                ..Default::default()
            };
            state.save(&path);
            state
        });
        // Threads made meanwhile (`/otter new`) are kept now.
        // Taken from memory only once kept, so a reply meanwhile finds it.
        let made: Vec<Pending> = self.slack.pending.lock().unwrap().clone();
        if !made.is_empty() {
            for p in &made {
                state.tasks.insert(p.post.task.clone(), p.after.clone());
                state.posts.push(p.post.clone());
            }
            state.save(&path);
            self.slack
                .pending
                .lock()
                .unwrap()
                .retain(|p| !made.contains(p));
        }
        // Presses meanwhile (D-207): kept, the newest for each task.
        let pressed: Vec<(String, Pressed)> = self
            .slack
            .pressed
            .lock()
            .unwrap()
            .iter()
            .map(|(t, p)| (t.clone(), p.clone()))
            .collect();
        if !pressed.is_empty() {
            for (task, p) in &pressed {
                if state.pressed.get(task).is_none_or(|kept| kept.at < p.at) {
                    state.pressed.insert(task.clone(), p.clone());
                }
            }
            state.save(&path);
            self.slack
                .pressed
                .lock()
                .unwrap()
                .retain(|t, p| !pressed.iter().any(|(kt, kp)| kt == t && kp == p));
        }
        self.slack_begun(c, env, &chat.messages, &mut state, &path)
            .await?;
        let look = self.slack_look(&state).await;
        self.slack_host(c, env, &chat.messages, &mut state, &look.titles, &path)
            .await?;
        self.slack_cards(c, env, &look, &mut state, &path).await?;
        self.slack_needs(c, env, &look, &mut state, &path).await?;
        self.slack_threads(c, env, &look, &mut state, &path).await?;
        self.slack_turns_ended(env, &c.bot, &look).await;
        self.slack_working(env, &c.bot, &look).await;
        Ok(())
    }

    /// A task Otter on the host made from the developer's message in Slack
    /// gets a message of its own at the top of that conversation (D-207),
    /// as `/otter new`'s does (D-202): its thread is the task's
    /// conversation, its request counts as said there, so what the task
    /// says after it goes there too. Only a task not over, made lately, that
    /// Slack has no message of yet: an otterd before this one made tasks it
    /// never told.
    async fn slack_begun(
        &self,
        c: &Config,
        env: &EnvMap,
        chat: &[Message],
        state: &mut State,
        path: &Path,
    ) -> Result<(), CallError> {
        use otter_core::task::TaskStatus;
        let now = chrono::Utc::now();
        let begun: Vec<(String, Origin, String, Option<String>, needs::Told, String)> = {
            let store = self.tasks.lock().await;
            store
                .iter()
                .filter_map(|t| {
                    let id = t.id.to_string();
                    if matches!(t.status, TaskStatus::Done | TaskStatus::Cancelled)
                        || state.posts.iter().any(|p| p.task == id)
                    {
                        return None;
                    }
                    let request = t.messages.iter().find(|m| m.role == MessageRole::User)?;
                    if now - request.at >= STALE {
                        return None;
                    }
                    let from = made_from(request, chat)?;
                    Some((
                        id,
                        from,
                        request.id.to_string(),
                        thread_after(&t.messages, request, now),
                        needs::card(t),
                        needs::card_key(t),
                    ))
                })
                .collect()
        };
        for (task, from, request, after, card, shows) in begun {
            let to = Origin {
                channel: from.channel,
                thread: None,
            };
            let body = |blocks: &Value| {
                let mut blocks = blocks.as_array().cloned().unwrap_or_default();
                labelled(&mut blocks);
                json!({
                    "channel": to.channel,
                    "text": card.text.chars().take(280).collect::<String>(),
                    "blocks": blocks,
                    "unfurl_links": false,
                    "unfurl_media": false,
                })
            };
            let mut sent = api::call(env, &c.bot, "chat.postMessage", &body(&card.blocks)).await;
            if let Err(CallError::Refused(e)) = &sent {
                tracing::warn!("slack: a task's buttons weren't taken: Slack said `{e}`");
                let words = json!([{ "type": "markdown", "text": format!("{}\n\n_Reply in this thread to talk to it._", card.text) }]);
                sent = api::call(env, &c.bot, "chat.postMessage", &body(&words)).await;
            }
            match sent {
                Ok(v) => {
                    state.posts.push(Post {
                        task: task.clone(),
                        key: "started".into(),
                        channel: to.channel.clone(),
                        ts: v["ts"].as_str().unwrap_or_default().to_owned(),
                        text: card.text.clone(),
                        open: false,
                        said: vec![request.clone()],
                        shows: Some(shows),
                    });
                    // What the task says after its request goes there.
                    state.tasks.insert(task, after);
                    state.save(path);
                }
                Err(e @ CallError::Unreachable(_)) => return Err(e),
                Err(e) => tracing::warn!("slack: a task's message wasn't posted: {e}"),
            }
        }
        Ok(())
    }

    /// Show each task's own message again once what it shows of the task
    /// changed (D-207): how it stands, and its buttons; a deleted task's,
    /// that it went.
    async fn slack_cards(
        &self,
        c: &Config,
        env: &EnvMap,
        look: &Look,
        state: &mut State,
        path: &Path,
    ) -> Result<(), CallError> {
        for (i, shows, card) in &look.cards {
            let p = state.posts[*i].clone();
            let mut blocks = card.blocks.as_array().cloned().unwrap_or_default();
            labelled(&mut blocks);
            let body = json!({
                "channel": p.channel,
                "ts": p.ts,
                "text": card.text.chars().take(280).collect::<String>(),
                "blocks": blocks,
            });
            match api::call(env, &c.bot, "chat.update", &body).await {
                Ok(_) => {}
                Err(e @ CallError::Unreachable(_)) => return Err(e),
                // Shown once: not tried again until it changes.
                Err(e) => tracing::warn!("slack: a task's message wasn't shown again: {e}"),
            }
            state.posts[*i].shows = Some(shows.clone());
            state.posts[*i].text = card.text.clone();
            state.save(path);
        }
        Ok(())
    }

    /// The tasks as Slack needs them, read at once.
    async fn slack_look(&self, state: &State) -> Look {
        let store = self.tasks.lock().await;
        let mut look = Look {
            at: Some(std::time::Instant::now()),
            ..Default::default()
        };
        for t in store.iter() {
            let id = t.id.to_string();
            look.titles.insert(id.clone(), t.title.clone());
            let need = t.needs();
            let key = need.as_ref().map(needs::key);
            let shows = needs::card_key(t);
            for (i, p) in state.posts.iter().enumerate() {
                if p.task == id && p.open && Some(&p.key) != key.as_ref() {
                    look.over.push((i, needs::over(t, &p.key)));
                }
                if p.task == id && p.key == "started" && p.shows.as_ref() != Some(&shows) {
                    look.cards.push((i, shows.clone(), needs::card(t)));
                }
            }
            let last = t.messages.last().map(|m| m.id.to_string());
            let looked = state.tasks.get(&id);
            if state.posts.iter().any(|p| p.task == id) && looked != Some(&last) {
                look.messages.insert(id.clone(), t.messages.clone());
            }
            if t.runs.iter().all(|r| r.state.is_over()) {
                look.quiet.insert(id.clone());
            }
            // At work, waiting on no one, its words going to a thread of
            // the developer's in Slack (D-211).
            if let Some(run) = t.live_run().filter(|r| {
                matches!(r.state, RunState::Starting | RunState::Running) && t.needs().is_none()
            }) && let Some(to) = spoke(&t.messages, chrono::Utc::now(), &state.heard(&id, None))
                .filter(|to| to.thread.is_some())
            {
                // (A live run's activity is in it, not yet in the task.)
                let last = self
                    .live_activity(&run.id)
                    .and_then(|(lines, _)| lines.last().cloned())
                    .or_else(|| run.activity.last().cloned());
                look.working.insert(id.clone(), (to, last));
            }
            look.last.insert(id.clone(), last);
            let form = need
                .as_ref()
                .filter(|n| n.kind == otter_core::task::NeedKind::Question)
                .and_then(|n| n.decision_id.as_ref())
                .and_then(|d| self.form_of(t.id.as_str(), d));
            look.needs.insert(
                id,
                need.map(|n| (needs::key(&n), needs::told(t, &n, form.as_deref()))),
            );
        }
        for (i, p) in state.posts.iter().enumerate() {
            if look.titles.contains_key(&p.task) {
                continue;
            }
            if p.open {
                look.over
                    .push((i, "No longer waiting: the task was deleted.".into()));
            }
            if p.key == "started" && p.shows.as_deref() != Some("deleted") {
                look.cards.push((i, "deleted".into(), needs::gone(&p.text)));
            }
        }
        look
    }

    /// Send Otter's messages on the host that Slack hasn't had: each that
    /// comes after a message the developer wrote in Slack, to where they
    /// wrote it.
    async fn slack_host(
        &self,
        c: &Config,
        env: &EnvMap,
        messages: &[Message],
        state: &mut State,
        titles: &HashMap<String, String>,
        path: &Path,
    ) -> Result<(), CallError> {
        let Some(from) = resume_at(messages, state.after.as_deref()) else {
            // Gone from the conversation (it started over): from its end.
            state.after = messages.last().map(|m| m.id.to_string());
            state.save(path);
            return Ok(());
        };
        let now = chrono::Utc::now();
        let notify = self.settings.read().unwrap().slack_notify();
        for (i, m) in messages.iter().enumerate().skip(from) {
            let to = (now - m.at < STALE)
                .then(|| destination(messages, i, &Heard::default()))
                .flatten();
            // That a task waits or stopped, the need's own message says,
            // with its buttons, while Slack tells needs: a follow-up saying
            // it too would say it twice (D-209, D-214).
            if notify && followed(m).is_some_and(|(_, key)| !key.starts_with("done:")) {
                state.after = Some(m.id.to_string());
                state.save(path);
                continue;
            }
            // Otter's word on a task with a thread of its own is said there
            // (D-207), and only there (D-209): that it is done; that it
            // waits or stopped, not at all — the need's own message says so,
            // with its buttons.
            let thread = followed(m).and_then(|(task, key)| {
                let post = state
                    .posts
                    .iter()
                    .find(|p| p.task == task && p.key == "started")?;
                Some((
                    post.channel.clone(),
                    post.ts.clone(),
                    key.starts_with("done:"),
                ))
            });
            if let (Some(_), Some((channel, ts, done))) = (&to, thread) {
                if done {
                    let there = Origin {
                        channel,
                        thread: Some(ts),
                    };
                    match post(env, &c.bot, &there, &m.text, titles).await {
                        Ok(()) => {}
                        Err(e @ CallError::Unreachable(_)) => return Err(e),
                        Err(e) => {
                            tracing::warn!("slack: Otter's message {} wasn't sent: {e}", m.id)
                        }
                    }
                }
                state.after = Some(m.id.to_string());
                state.save(path);
                continue;
            }
            if to.is_some() || m.role == MessageRole::Controller {
                match self
                    .slack_answer(env, &c.bot, None, to.as_ref(), m, titles)
                    .await
                {
                    Ok(()) => {}
                    Err(e @ CallError::Unreachable(_)) => return Err(e),
                    // Never sent: said, and not tried again for ever.
                    Err(e) => tracing::warn!("slack: Otter's message {} wasn't sent: {e}", m.id),
                }
            }
            state.after = Some(m.id.to_string());
            state.save(path);
        }
        Ok(())
    }

    /// Tell Slack each new need, and take the buttons off a need that is
    /// over, saying what came of it (D-201).
    async fn slack_needs(
        &self,
        c: &Config,
        env: &EnvMap,
        look: &Look,
        state: &mut State,
        path: &Path,
    ) -> Result<(), CallError> {
        if state.needs.is_none() {
            // The first look (`slack_begin` missed it): what waits already
            // is the baseline.
            state.needs = Some(baseline(look));
            state.save(path);
            return Ok(());
        }
        for (i, what) in &look.over {
            let p = state.posts[*i].clone();
            let mut blocks = vec![
                json!({ "type": "markdown", "text": p.text }),
                json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": what }] }),
            ];
            labelled(&mut blocks);
            let body = json!({
                "channel": p.channel,
                "ts": p.ts,
                "text": p.text.chars().take(280).collect::<String>(),
                "blocks": blocks,
            });
            match api::call(env, &c.bot, "chat.update", &body).await {
                Ok(_) => {}
                Err(e @ CallError::Unreachable(_)) => return Err(e),
                Err(e) => tracing::warn!("slack: a need's message wasn't updated: {e}"),
            }
            state.posts[*i].open = false;
            state.save(path);
            // A form there has nothing more to submit.
            self.slack
                .picks
                .lock()
                .unwrap()
                .retain(|(m, _), _| m != &p.ts);
        }
        // Read now: turning it on or off needs no new connection.
        let notify = self.settings.read().unwrap().slack_notify();
        for (task, need) in &look.needs {
            let seen = state.needs.as_ref().and_then(|n| n.get(task)).cloned();
            let Some((key, told)) = need else {
                if seen.is_some() {
                    state.needs.as_mut().map(|n| n.remove(task));
                    state.save(path);
                }
                continue;
            };
            if seen.as_ref() == Some(key) {
                continue;
            }
            // Still waiting, however long ago it began: told, also after
            // Slack was away.
            if notify {
                for user in &c.users {
                    let channel = self.slack_dm(env, &c.bot, user).await?;
                    let posted =
                        |p: &Post| &p.task == task && &p.key == key && p.channel == channel;
                    if state.posts.iter().any(posted) {
                        continue;
                    }
                    let body = |blocks: &Value| {
                        let mut blocks = blocks.as_array().cloned().unwrap_or_default();
                        labelled(&mut blocks);
                        json!({
                            "channel": channel,
                            "text": told.text.chars().take(280).collect::<String>(),
                            "blocks": blocks,
                            "unfurl_links": false,
                            "unfurl_media": false,
                        })
                    };
                    let mut sent =
                        api::call(env, &c.bot, "chat.postMessage", &body(&told.blocks)).await;
                    // A block Slack won't take mustn't leave the need untold:
                    // its words alone, then.
                    if let Err(CallError::Refused(e)) = &sent {
                        tracing::warn!("slack: a need's buttons weren't taken: Slack said `{e}`");
                        let words = format!("{}\n\n_{UNSHOWN}_", told.text);
                        let blocks = json!([{ "type": "markdown", "text": words }]);
                        sent = api::call(env, &c.bot, "chat.postMessage", &body(&blocks)).await;
                    }
                    match sent {
                        Ok(v) => {
                            state.posts.push(Post {
                                task: task.clone(),
                                key: key.clone(),
                                channel: channel.clone(),
                                ts: v["ts"].as_str().unwrap_or_default().to_owned(),
                                text: told.text.clone(),
                                open: true,
                                said: vec![],
                                shows: None,
                            });
                            // Its thread starts where the conversation is now.
                            state
                                .tasks
                                .entry(task.clone())
                                .or_insert_with(|| look.last.get(task).cloned().flatten());
                            // Kept at once: a failure telling the next member
                            // mustn't lose this one.
                            state.save(path);
                        }
                        Err(e @ CallError::Unreachable(_)) => return Err(e),
                        Err(e) => tracing::warn!("slack: a need wasn't told: {e}"),
                    }
                }
                capped(&mut state.posts, |task| look.titles.contains_key(task));
            }
            state
                .needs
                .get_or_insert_default()
                .insert(task.clone(), key.clone());
            state.save(path);
        }
        // Deleted tasks need nothing.
        if let Some(seen) = state.needs.as_mut() {
            let before = seen.len();
            seen.retain(|t, _| look.needs.contains_key(t));
            if seen.len() != before {
                state.save(path);
            }
        }
        Ok(())
    }

    /// Send Otter's and the coding agent's messages in a task's
    /// conversation that came after the developer's in its Slack thread,
    /// into that thread (D-201).
    async fn slack_threads(
        &self,
        c: &Config,
        env: &EnvMap,
        look: &Look,
        state: &mut State,
        path: &Path,
    ) -> Result<(), CallError> {
        let now = chrono::Utc::now();
        for (task, messages) in &look.messages {
            let Some(after) = state.tasks.get(task).cloned() else {
                // Not seen yet: from where it is now.
                state
                    .tasks
                    .insert(task.clone(), messages.last().map(|m| m.id.to_string()));
                state.save(path);
                continue;
            };
            let Some(from) = resume_at(messages, after.as_deref()) else {
                state
                    .tasks
                    .insert(task.clone(), messages.last().map(|m| m.id.to_string()));
                state.save(path);
                continue;
            };
            // (A press is kept before what it does: one since this sync
            // began is in memory still.)
            let pressed = self.slack.pressed.lock().unwrap().get(task).cloned();
            let mut heard = state.heard(task, pressed.as_ref());
            for (i, m) in messages.iter().enumerate().skip(from) {
                // The developer's words Otter on the host said here, from
                // their message in Slack (D-213): shown in the task's
                // thread, and counted as said there, so what the task says
                // next goes there too (as `made_from`, D-207).
                if let Some(said_from) = relayed(m)
                    && now - m.at < STALE
                    && let Some(p) = thread_of(&state.posts, task)
                    && self.said_in_slack(said_from).await
                {
                    let there = Origin {
                        channel: state.posts[p].channel.clone(),
                        thread: Some(state.posts[p].ts.clone()),
                    };
                    match post(env, &c.bot, &there, &relay(&m.text), &look.titles).await {
                        Ok(()) => {}
                        Err(e @ CallError::Unreachable(_)) => return Err(e),
                        Err(e) => tracing::warn!("slack: a message of {task} wasn't shown: {e}"),
                    }
                    state.posts[p].said.push(m.id.to_string());
                    heard.said.insert(m.id.to_string(), there);
                    state.tasks.insert(task.clone(), Some(m.id.to_string()));
                    state.save(path);
                    continue;
                }
                let to = (now - m.at < STALE)
                    .then(|| destination(messages, i, &heard))
                    .flatten();
                if to.is_some() || m.role != MessageRole::User {
                    match self
                        .slack_answer(env, &c.bot, Some(task), to.as_ref(), m, &look.titles)
                        .await
                    {
                        Ok(()) => {}
                        Err(e @ CallError::Unreachable(_)) => return Err(e),
                        Err(e) => tracing::warn!("slack: a message of {task} wasn't sent: {e}"),
                    }
                }
                state.tasks.insert(task.clone(), Some(m.id.to_string()));
                state.save(path);
            }
        }
        // Tasks gone, and threads no longer kept, are let go.
        state
            .tasks
            .retain(|t, _| state.posts.iter().any(|p| &p.task == t));
        state
            .pressed
            .retain(|t, _| state.posts.iter().any(|p| &p.task == t));
        Ok(())
    }

    /// The coding agent's words shown for a turn that ended with no message
    /// of its own (the same words again): left as last said, whole, and
    /// the next turn shown afresh, where the developer is then (D-203).
    async fn slack_turns_ended(&self, env: &EnvMap, bot: &str, look: &Look) {
        let read = look.at.unwrap_or_else(std::time::Instant::now);
        let ended: Vec<Stream> = {
            let mut streams = self.slack.streams.lock().unwrap();
            // A deleted task's go with it.
            streams.retain(|k, _| {
                k.strip_prefix("agent:")
                    .is_none_or(|task| look.titles.contains_key(task))
            });
            let now = std::time::Instant::now();
            let mut ended = Vec::new();
            for task in look.titles.keys() {
                let key = format!("agent:{task}");
                let Some(s) = streams.get_mut(&key) else {
                    continue;
                };
                if !look.quiet.contains(task) {
                    // A turn going on, however long silent (a long build):
                    // its message is kept.
                    s.seen = now;
                } else if !s.posting && s.seen <= read {
                    // (Words since the tasks were read are a new turn's.)
                    ended.extend(streams.remove(&key));
                }
            }
            ended
        };
        for s in ended {
            // (Shown at work, the words it has without that, D-211.)
            let (Some(to), Some(ts), true) = (s.to, s.ts, !s.whole || s.busy.is_some()) else {
                continue;
            };
            let text = socket::pieces(&s.text, MAX_PIECE)
                .first()
                .cloned()
                .unwrap_or_default();
            if text.is_empty() {
                // Only ever shown at work: nothing of it is left.
                let body = json!({ "channel": to.channel, "ts": ts });
                if let Err(e) = api::call(env, bot, "chat.delete", &body).await {
                    tracing::info!("slack: the agent shown at work wasn't taken away: {e}");
                }
                continue;
            }
            let body = json!({
                "channel": to.channel,
                "ts": ts,
                "text": text.chars().take(280).collect::<String>(),
                "blocks": shown(&text, &to),
            });
            if let Err(e) = api::call(env, bot, "chat.update", &body).await {
                tracing::info!("slack: the agent's last words weren't shown whole: {e}");
            }
        }
    }

    /// A coding agent at work that has said nothing for a while is shown at
    /// it in its task's thread (D-211), so the developer knows it goes on:
    /// under its words so far, or in a message of its own that its words
    /// then take the place of — how long it has been quiet, and what it
    /// did last. Its next words, its turn's message, waiting on someone or
    /// its run's end (`slack_turns_ended`) take that away.
    async fn slack_working(&self, env: &EnvMap, bot: &str, look: &Look) {
        let todo = busy_plan(
            &mut self.slack.streams.lock().unwrap(),
            look,
            std::time::Instant::now(),
            agent_quiet(),
        );
        for (key, next) in todo {
            let posted = match &next {
                Busy::Post(to, words) => {
                    let mut body = json!({
                        "channel": to.channel,
                        "text": words.chars().take(280).collect::<String>(),
                        "blocks": shown(words, to),
                        "unfurl_links": false,
                        "unfurl_media": false,
                    });
                    if let Some(t) = &to.thread {
                        body["thread_ts"] = json!(t);
                    }
                    api::call(env, bot, "chat.postMessage", &body).await
                }
                Busy::Update(to, ts, words) => {
                    let body = json!({
                        "channel": to.channel,
                        "ts": ts,
                        "text": words.chars().take(280).collect::<String>(),
                        "blocks": shown(words, to),
                    });
                    api::call(env, bot, "chat.update", &body).await
                }
                Busy::Delete(to, ts) => {
                    let body = json!({ "channel": to.channel, "ts": ts });
                    api::call(env, bot, "chat.delete", &body).await
                }
            };
            if let Err(e) = &posted {
                tracing::info!("slack: the coding agent wasn't shown at work: {e}");
            }
            let mut streams = self.slack.streams.lock().unwrap();
            if let Some(s) = streams.get_mut(&key) {
                s.posting = false;
                match (&next, &posted) {
                    (Busy::Post(..), Ok(v)) => s.ts = v["ts"].as_str().map(String::from),
                    // Gone only once Slack says so: else it is still there to
                    // show again, not a second one.
                    (Busy::Delete(..), Ok(_)) => s.ts = None,
                    _ => {}
                }
            }
        }
    }

    /// Where the developer spoke to `task` from Slack, besides their
    /// messages' own `correlation_id`s (D-207): as kept, and pressed since.
    fn slack_heard_by(&self, task: &str) -> Heard {
        let path = self.paths.state_dir.join("slack.json");
        let pressed = self.slack.pressed.lock().unwrap().get(task).cloned();
        State::load(&path)
            .unwrap_or_default()
            .heard(task, pressed.as_ref())
    }

    /// Whether the host's message `id` was said in Slack.
    async fn said_in_slack(&self, id: &str) -> bool {
        self.chat
            .lock()
            .await
            .get()
            .messages
            .iter()
            .rfind(|m| m.id.as_str() == id)
            .is_some_and(|m| Origin::of(m.correlation_id.as_deref()).is_some())
    }

    /// Whether the developer talks to `f` from Slack now (D-207): what it
    /// says next goes there.
    pub(crate) fn on_slack(&self, f: &otter_core::task::Task) -> bool {
        let heard = self.slack_heard_by(f.id.as_str());
        spoke(&f.messages, chrono::Utc::now(), &heard).is_some()
    }

    /// The direct messages between `user` and the app.
    async fn slack_dm(&self, env: &EnvMap, bot: &str, user: &str) -> Result<String, CallError> {
        if let Some(c) = self.slack.channels.lock().unwrap().get(user) {
            return Ok(c.clone());
        }
        let v = api::call(env, bot, "conversations.open", &json!({ "users": user })).await?;
        let channel = v["channel"]["id"]
            .as_str()
            .ok_or_else(|| CallError::Refused("no_channel".into()))?
            .to_owned();
        self.slack
            .channels
            .lock()
            .unwrap()
            .insert(user.to_owned(), channel.clone());
        Ok(channel)
    }
}

/// The pictures of a message, fetched (D-202): each file's first address
/// that gives a picture Otter takes, within a message's bounds (D-171); and
/// how many files were left out.
async fn pictures(env: &EnvMap, bot: &str, files: &[Vec<String>]) -> (Vec<TaskImage>, usize) {
    use base64::Engine;
    use otter_protocol::task::{MAX_IMAGE_BYTES, MAX_IMAGES, MAX_IMAGES_TOTAL};
    let (mut out, mut total, mut left) = (Vec::new(), 0, 0);
    for addresses in files {
        let mut got = None;
        if out.len() < MAX_IMAGES {
            for url in addresses {
                match api::download(env, bot, url, MAX_IMAGE_BYTES).await {
                    Ok(bytes) if bytes.len() <= MAX_IMAGE_BYTES => {
                        if let Some(kind) = socket::picture_type(&bytes) {
                            got = Some((kind, bytes));
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::info!("slack: a picture wasn't fetched: {e}"),
                }
            }
        }
        match got {
            Some((kind, bytes)) if total + bytes.len() <= MAX_IMAGES_TOTAL => {
                total += bytes.len();
                out.push(TaskImage {
                    media_type: kind.into(),
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                });
            }
            _ => left += 1,
        }
    }
    (out, left)
}

/// The question a reply under `under` answers: the one that message asked,
/// while it is the task's open question with no options. A reply under any
/// other need's message is said in the task.
fn answers(under: &Post, open: Option<DecisionId>) -> Option<DecisionId> {
    open.filter(|d| under.key == format!("question:{d}"))
}

/// Tell Slack what came of the developer's message: 👀, or why not.
async fn mark(env: &EnvMap, bot: &str, m: &socket::Inbound, sent: RpcResult<()>) {
    match sent {
        Ok(()) => {
            let seen = json!({ "channel": m.to.channel, "timestamp": m.ts, "name": "eyes" });
            match api::call(env, bot, "reactions.add", &seen).await {
                Ok(_) | Err(CallError::Refused(_)) => {}
                Err(e) => tracing::warn!("slack: marking a message seen: {e}"),
            }
        }
        Err(e) => {
            let line = format!("Otter didn't take that: {}", e.message);
            if let Err(e) = post(env, bot, &m.to, &line, &HashMap::new()).await {
                tracing::warn!("slack: {e}");
            }
        }
    }
}

/// What an event on the host is to Slack: something to look at
/// (`Some(true)`, also when some were missed), nothing (`Some(false)`), or
/// the end of the host's events (`None`).
fn worth_a_look(
    ev: Result<otter_protocol::events::EventRecord, tokio::sync::broadcast::error::RecvError>,
) -> Option<bool> {
    use tokio::sync::broadcast::error::RecvError;
    match ev {
        Ok(r) => Some(matches!(
            r.event,
            Event::OtterChanged { .. } | Event::TaskChanged { .. } | Event::TaskDeleted { .. }
        )),
        Err(RecvError::Lagged(_)) => Some(true),
        Err(RecvError::Closed) => None,
    }
}

/// The needs that wait now, as already told: not news.
fn baseline(look: &Look) -> BTreeMap<String, String> {
    look.needs
        .iter()
        .filter_map(|(id, n)| Some((id.clone(), n.as_ref()?.0.clone())))
        .collect()
}

/// Where sending to Slack picks up in a conversation: after `after`, or
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

/// Where a conversation's `i`th message goes in Slack: Otter's, or the
/// coding agent's (its words reach the developer as written, D-086), when
/// the developer's latest message before it came from Slack.
fn destination(messages: &[Message], i: usize, heard: &Heard) -> Option<Origin> {
    if !matches!(
        messages[i].role,
        MessageRole::Controller | MessageRole::Agent
    ) {
        return None;
    }
    spoke(&messages[..i], messages[i].at, heard)
}

/// Where the developer last spoke to a conversation from Slack, before
/// `at`: their latest message's place, or — pressed since — the thread of
/// the message they pressed on (D-207). `None`: they last spoke elsewhere.
fn spoke(messages: &[Message], at: otter_core::Timestamp, heard: &Heard) -> Option<Origin> {
    let last = messages.iter().rev().find(|m| m.role == MessageRole::User);
    match &heard.pressed {
        Some(p) if p.at <= at && last.is_none_or(|m| m.at < p.at) => Some(p.origin()),
        _ => heard.origin(last?),
    }
}

/// What to do to show a coding agent at work (D-211).
#[derive(Debug, PartialEq)]
enum Busy {
    Post(Origin, String),
    Update(Origin, String, String),
    Delete(Origin, String),
}

/// What `slack_working` does now, decided under the streams' lock: each
/// working task's stream made if need be, and shown at work once quiet
/// for `quiet`; each other's no longer shown so, and its quiet counted
/// again from when it is at work again — a wait on the developer isn't the
/// agent's silence. What is decided is marked `posting` until done.
fn busy_plan(
    streams: &mut HashMap<String, Stream>,
    look: &Look,
    now: std::time::Instant,
    quiet: Duration,
) -> Vec<(String, Busy)> {
    let mut todo = Vec::new();
    for (task, (to, last)) in &look.working {
        let key = format!("agent:{task}");
        let s = streams.entry(key.clone()).or_insert_with(|| Stream {
            to: Some(to.clone()),
            ts: None,
            posting: false,
            over: false,
            text: String::new(),
            shown: None,
            whole: false,
            seen: now,
            heard: now,
            busy: None,
        });
        let Some(to) = s.to.clone().filter(|_| !s.over && !s.posting) else {
            continue;
        };
        let still = now.saturating_duration_since(s.heard);
        let busy = busy_line(still, last.as_deref());
        if still < quiet
            || s.busy.as_ref() == Some(&busy)
            || s.busy.is_some()
                && s.shown
                    .is_some_and(|t| now.saturating_duration_since(t) < BUSY_EVERY)
        {
            continue;
        }
        let words = with_busy(&s.text, &busy);
        s.busy = Some(busy);
        s.posting = true;
        s.shown = Some(now);
        todo.push((
            key,
            match &s.ts {
                None => Busy::Post(to, words),
                Some(ts) => Busy::Update(to, ts.clone(), words),
            },
        ));
    }
    // Not at work now (it waits on someone, its words go elsewhere, or its
    // run is over): what showed it at work is taken away — a run over is
    // `slack_turns_ended`'s.
    for (key, s) in streams.iter_mut() {
        let Some(task) = key.strip_prefix("agent:") else {
            continue;
        };
        if look.working.contains_key(task) {
            continue;
        }
        s.heard = now;
        if look.quiet.contains(task) || s.posting {
            continue;
        }
        let (Some(to), Some(ts), Some(_)) = (s.to.clone(), s.ts.clone(), s.busy.take()) else {
            continue;
        };
        s.posting = true;
        todo.push((
            key.clone(),
            match socket::pieces(&s.text, MAX_PIECE).first() {
                None => Busy::Delete(to, ts),
                Some(words) => Busy::Update(to, ts, words.clone()),
            },
        ));
    }
    todo
}

/// How long the coding agent says nothing before its task's thread says it
/// is at work (D-211).
fn agent_quiet() -> Duration {
    crate::controller::env_ms("OTTER_SLACK_AGENT_QUIET_MS", 2 * 60 * 1000)
}

/// What a task's thread says of its coding agent at work, quiet (D-211):
/// for how long, and what it did last.
fn busy_line(still: Duration, last: Option<&str>) -> String {
    let since = match still.as_secs() / 60 {
        0 => "less than a minute".to_owned(),
        1 => "a minute".to_owned(),
        n => format!("{n} minutes"),
    };
    let mut line = format!("_The coding agent is working… nothing said for {since}._");
    if let Some(last) = last.filter(|l| !l.trim().is_empty()) {
        let last = otter_core::redact::redact(last);
        if last.contains('`') {
            line.push_str(&format!("\nLatest: {last}"));
        } else {
            line.push_str(&format!("\nLatest: `{last}`"));
        }
    }
    line
}

/// The coding agent's words so far with what it is shown at under them, or
/// that alone (D-211).
fn with_busy(text: &str, busy: &str) -> String {
    match socket::pieces(text, MAX_PIECE - busy.len() - 8).first() {
        Some(words) => format!("{words} …\n\n{busy}"),
        None => busy.to_owned(),
    }
}

/// Post `text` (Markdown) to `to`, in as many messages as it takes, each
/// task named by its title.
async fn post(
    env: &EnvMap,
    bot: &str,
    to: &Origin,
    text: &str,
    titles: &HashMap<String, String>,
) -> Result<(), CallError> {
    let text = needs::titled(&otter_core::redact::redact(text), titles);
    for piece in socket::pieces(&text, MAX_PIECE) {
        let mut body = json!({
            "channel": to.channel,
            // What a notification shows.
            "text": piece.chars().take(280).collect::<String>(),
            "blocks": shown(&piece, to),
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

/// A message's blocks: its Markdown, and — at the top of the conversation,
/// from a host otterhub serves — the host's name (D-207): one app speaks for
/// several hosts there.
fn shown(text: &str, to: &Origin) -> Value {
    let mut blocks = vec![json!({ "type": "markdown", "text": text })];
    if to.thread.is_none() {
        labelled(&mut blocks);
    }
    Value::Array(blocks)
}

/// The host's name under a message at the top of the conversation, when
/// otterhub serves it (D-207).
fn labelled(blocks: &mut Vec<Value>) {
    if let Some(name) = LABEL.lock().unwrap().clone() {
        blocks.push(json!({
            "type": "context",
            "elements": [{ "type": "mrkdwn", "text": format!("from *{name}*") }],
        }));
    }
}

/// The name otterhub knows this host by, while it serves it (D-207): what
/// this host's messages at the top of the conversation are labelled with.
static LABEL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Where the developer wrote the message in Slack that Otter on the host
/// made a task from (D-207): the task's request was made under
/// `<their message>:create_task…` (`gateway.rs`).
fn made_from(request: &Message, chat: &[Message]) -> Option<Origin> {
    let (id, _) = request
        .correlation_id
        .as_deref()?
        .split_once(":create_task")?;
    let m = chat
        .iter()
        .find(|m| m.id.as_str() == id && m.role == MessageRole::User)?;
    Origin::of(m.correlation_id.as_deref())
}

/// Where a task's new thread picks up (D-207): after its request, for a
/// task made just now — what it said since goes there; else after its last
/// message: one made a while ago (by an otterd before this one) isn't told
/// its whole story.
fn thread_after(
    messages: &[Message],
    request: &Message,
    now: otter_core::Timestamp,
) -> Option<String> {
    match now - request.at < FRESH {
        true => Some(request.id.to_string()),
        false => messages.last().map(|m| m.id.to_string()),
    }
}

/// The messages kept, at most `MAX_POSTS`: the oldest go, but a task's own
/// message while its task is there (`live`) — its thread is the task's
/// (D-207).
fn capped(posts: &mut Vec<Post>, live: impl Fn(&str) -> bool) {
    let mut extra = posts.len().saturating_sub(MAX_POSTS);
    posts.retain(|p| {
        if extra == 0 || (p.key == "started" && live(&p.task)) {
            return true;
        }
        extra -= 1;
        false
    });
}

/// The request of a task Otter on the host made a moment ago (D-207).
fn just_made(m: &Message) -> bool {
    m.role == MessageRole::User
        && m.correlation_id
            .as_deref()
            .is_some_and(|c| c.contains(":create_task"))
        && chrono::Utc::now() - m.at < FRESH
}

/// The host's message a task's message was said from by Otter on the host
/// (D-213): the developer's own, made `<message>:send_to_task:<task>`.
fn relayed(m: &Message) -> Option<&str> {
    if m.role != MessageRole::User {
        return None;
    }
    let (from, _) = m.correlation_id.as_deref()?.split_once(":send_to_task:")?;
    Some(from)
}

/// The message whose thread is a task's conversation in Slack (D-207): its
/// own, else its latest need's.
fn thread_of(posts: &[Post], task: &str) -> Option<usize> {
    posts
        .iter()
        .position(|p| p.task == task && p.key == "started")
        .or_else(|| posts.iter().rposition(|p| p.task == task))
}

/// How the developer's words said for them in a task look in its thread
/// (D-213): theirs, said to Otter elsewhere.
fn relay(text: &str) -> String {
    format!("_You, to Otter:_\n{text}")
}

/// A follow-up of Otter's on the host (D-094): its task and what changed.
fn followed(m: &Message) -> Option<(&str, &str)> {
    m.correlation_id
        .as_deref()?
        .strip_prefix("follow:")?
        .split_once(':')
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
            .map(|i| {
                destination(&m, i, &Heard::default())
                    .map(|o| format!("{}{:?}", o.channel, o.thread))
            })
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

    /// A quiet coding agent is shown at work (D-211): for how long, what it
    /// did last (as code, unless that has backticks of its own), under its
    /// words so far.
    #[test]
    fn a_quiet_agent_is_shown_at_work() {
        let secs = Duration::from_secs;
        assert_eq!(
            busy_line(secs(20), None),
            "_The coding agent is working… nothing said for less than a minute._"
        );
        assert_eq!(
            busy_line(secs(70), Some("$ cargo test")),
            "_The coding agent is working… nothing said for a minute._\nLatest: `$ cargo test`"
        );
        assert_eq!(
            busy_line(secs(185), Some("$ echo `date`")),
            "_The coding agent is working… nothing said for 3 minutes._\nLatest: $ echo `date`"
        );
        assert_eq!(with_busy("", "B"), "B");
        assert_eq!(with_busy("Looking at it", "B"), "Looking at it …\n\nB");
    }

    /// When a quiet coding agent is shown at work (D-211): once quiet for
    /// the time, in a message of its own; taken away when it waits on the
    /// developer — deleted, being all that message had, and only once Slack
    /// says so — and a wait isn't counted as its quiet once it works again.
    #[test]
    fn a_wait_is_not_the_agents_quiet() {
        let quiet = Duration::from_secs(120);
        let t0 = std::time::Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let to = Origin {
            channel: "D1".into(),
            thread: Some("1.5".into()),
        };
        let mut working = Look::default();
        working
            .working
            .insert("ft_a".into(), (to.clone(), Some("$ make check".into())));
        let waiting = Look::default();
        let mut streams = HashMap::new();
        // At work: nothing yet, then shown at work in a message of its own.
        assert_eq!(busy_plan(&mut streams, &working, at(0), quiet), vec![]);
        let shown = busy_plan(&mut streams, &working, at(121), quiet);
        let line = busy_line(Duration::from_secs(121), Some("$ make check"));
        assert_eq!(
            shown,
            vec![("agent:ft_a".into(), Busy::Post(to.clone(), line))]
        );
        let s = streams.get_mut("agent:ft_a").unwrap();
        s.posting = false;
        s.ts = Some("1.6".into());
        // It waits on the developer: taken away; its message is still there
        // until the delete is done.
        let gone = busy_plan(&mut streams, &waiting, at(130), quiet);
        assert_eq!(
            gone,
            vec![("agent:ft_a".into(), Busy::Delete(to.clone(), "1.6".into()))]
        );
        let s = streams.get_mut("agent:ft_a").unwrap();
        assert_eq!(s.ts.as_deref(), Some("1.6"));
        s.posting = false;
        s.ts = None;
        // Half an hour later the developer answers: it works again, and
        // isn't shown "quiet for 31 minutes" at once.
        assert_eq!(busy_plan(&mut streams, &waiting, at(1900), quiet), vec![]);
        assert_eq!(busy_plan(&mut streams, &working, at(1901), quiet), vec![]);
        let again = busy_plan(&mut streams, &working, at(2030), quiet);
        assert!(
            matches!(&again[..], [(_, Busy::Post(_, l))] if l.contains("for 2 minutes")),
            "{again:?}"
        );
    }

    /// In a task's thread the coding agent's words go back too (D-086,
    /// D-201); a status line of the system's doesn't.
    #[test]
    fn the_coding_agents_words_go_back_to_the_thread_too() {
        use MessageRole::{Agent, Controller, System, User};
        let m = vec![
            msg("m0", User, Some("slack:D1:2.0:1.5")),
            msg("m1", Agent, None),
            msg("m2", System, None),
            msg("m3", Controller, Some("m0")),
        ];
        let to: Vec<bool> = (0..m.len())
            .map(|i| destination(&m, i, &Heard::default()).is_some())
            .collect();
        assert_eq!(to, vec![false, true, false, true]);
        assert_eq!(
            destination(&m, 1, &Heard::default())
                .unwrap()
                .thread
                .as_deref(),
            Some("1.5")
        );
    }

    /// A reply belongs to the task whose need's message it is under; one
    /// at the top, or under anything else, belongs to no task.
    #[test]
    fn a_reply_belongs_to_the_task_whose_need_it_is_under() {
        let post = |task: &str, ts: &str| Post {
            task: task.into(),
            key: "decision:dec_1".into(),
            channel: "D1".into(),
            ts: ts.into(),
            text: String::new(),
            open: true,
            said: vec![],
            shows: None,
        };
        let state = State {
            posts: vec![post("ft_a", "1.5"), post("ft_b", "2.5")],
            ..Default::default()
        };
        let on = |channel: &str, thread: Option<&str>| Origin {
            channel: channel.into(),
            thread: thread.map(String::from),
        };
        let task = |o: Origin| state.post_of(&o).map(|p| p.task);
        assert_eq!(task(on("D1", Some("2.5"))).as_deref(), Some("ft_b"));
        assert_eq!(task(on("D1", None)), None);
        assert_eq!(task(on("D1", Some("9.9"))), None);
        assert_eq!(task(on("D2", Some("1.5"))), None);
    }

    /// A reply answers a question only under that question's own message;
    /// under another need's message of the same task, it is said in the
    /// task.
    #[test]
    fn a_reply_answers_only_the_question_it_is_under() {
        let under = |key: &str| Post {
            task: "ft_a".into(),
            key: key.into(),
            channel: "D1".into(),
            ts: "1.5".into(),
            text: String::new(),
            open: true,
            said: vec![],
            shows: None,
        };
        let open = || Some(DecisionId::from("dec_q"));
        assert_eq!(answers(&under("question:dec_q"), open()), open());
        assert_eq!(answers(&under("decision:dec_1"), open()), None);
        assert_eq!(answers(&under("question:dec_old"), open()), None);
        assert_eq!(answers(&under("question:dec_q"), None), None);
    }

    /// A task Otter on the host made from a message in Slack (D-207): its
    /// request counts as said in its own message's thread, so what it says
    /// after goes there. A press later sends what follows to the message
    /// pressed on; the developer's next message, from wherever it is,
    /// takes over again.
    #[test]
    fn a_task_made_from_slack_speaks_in_its_thread_and_where_pressed() {
        use MessageRole::{Agent, Controller, User};
        let t0 = chrono::Utc::now();
        let at = |s: i64, mut m: Message| {
            m.at = t0 + chrono::TimeDelta::seconds(s);
            m
        };
        let m = vec![
            at(0, msg("req", User, Some("msg_h1:create_task:0a1b2c3d"))),
            at(1, msg("m1", Controller, None)),
            at(2, msg("m2", Agent, None)),
            at(4, msg("m3", Controller, Some("slack:act:1.1"))),
            at(5, msg("m4", User, Some("cmd-app-1"))),
            at(6, msg("m5", Controller, Some("m4"))),
        ];
        let state = State {
            posts: vec![Post {
                task: "ft_a".into(),
                key: "started".into(),
                channel: "D1".into(),
                ts: "1.5".into(),
                text: String::new(),
                open: false,
                said: vec!["req".into()],
                shows: None,
            }],
            ..Default::default()
        };
        let place = |heard: &Heard, i: usize| {
            destination(&m, i, heard).map(|o| format!("{}:{}", o.channel, o.thread.unwrap()))
        };
        // Without its thread: nowhere (what an otterd before this did).
        assert_eq!(place(&Heard::default(), 1), None);
        let heard = state.heard("ft_a", None);
        assert_eq!(place(&heard, 1).as_deref(), Some("D1:1.5"));
        assert_eq!(place(&heard, 2).as_deref(), Some("D1:1.5"));
        assert_eq!(state.heard("ft_b", None).said.len(), 0);
        // Pressed on a need's message at 3 s: what follows goes there…
        let press = Pressed {
            channel: "D1".into(),
            ts: "2.5".into(),
            at: t0 + chrono::TimeDelta::seconds(3),
        };
        let heard = state.heard("ft_a", Some(&press));
        assert_eq!(place(&heard, 2).as_deref(), Some("D1:1.5"));
        assert_eq!(place(&heard, 3).as_deref(), Some("D1:2.5"));
        // …until the developer speaks from the app.
        assert_eq!(place(&heard, 5), None);
        // A press kept and a newer one in memory: the newer.
        let kept = State {
            pressed: [(
                "ft_a".to_owned(),
                Pressed {
                    ts: "0.5".into(),
                    at: t0,
                    ..press.clone()
                },
            )]
            .into(),
            ..Default::default()
        };
        assert_eq!(kept.heard("ft_a", Some(&press)).pressed.unwrap().ts, "2.5");
        assert_eq!(kept.heard("ft_a", None).pressed.unwrap().ts, "0.5");
    }

    /// A task's new thread: what it said since its request, made just now;
    /// from its last message, made a while ago (D-207). A task's own
    /// message outlasts the cap while its task is there.
    #[test]
    fn a_new_thread_starts_where_it_should_and_outlasts_the_cap() {
        use MessageRole::{Agent, User};
        let now = chrono::Utc::now();
        let mut request = msg("req", User, Some("msg_h1:create_task:0a1b2c3d"));
        let messages = vec![request.clone(), msg("m1", Agent, None)];
        assert_eq!(
            thread_after(&messages, &request, now).as_deref(),
            Some("req")
        );
        request.at = now - chrono::TimeDelta::hours(3);
        assert_eq!(
            thread_after(&messages, &request, now).as_deref(),
            Some("m1")
        );
        let post = |task: &str, key: &str| Post {
            task: task.into(),
            key: key.into(),
            channel: "D1".into(),
            ts: "1.5".into(),
            text: String::new(),
            open: false,
            said: vec![],
            shows: None,
        };
        let mut posts = vec![post("ft_live", "started"), post("ft_gone", "started")];
        posts.extend((0..MAX_POSTS).map(|_| post("ft_x", "decision:d")));
        capped(&mut posts, |t| t == "ft_live");
        assert_eq!(posts.len(), MAX_POSTS);
        assert_eq!(posts[0].task, "ft_live");
        assert!(posts.iter().all(|p| p.task != "ft_gone"));
        assert_eq!(
            posts.iter().filter(|p| p.task == "ft_x").count(),
            MAX_POSTS - 1
        );
    }

    /// The developer's message a task was made from on the host, and where
    /// it was written in Slack: only one written there, only Otter's
    /// `create_task` (D-207).
    #[test]
    fn a_task_made_on_the_host_knows_the_slack_message_it_came_from() {
        use MessageRole::{Controller, User};
        let chat = vec![
            msg("msg_h1", User, Some("slack:D1:1700.1")),
            msg("msg_h2", User, Some("cli-1")),
            msg("msg_h3", Controller, Some("slack:D1:1700.2")),
        ];
        let request = |c: &str| msg("req", User, Some(c));
        let from = |c: &str| made_from(&request(c), &chat).map(|o| (o.channel, o.thread));
        assert_eq!(
            from("msg_h1:create_task:0a1b2c3d"),
            Some(("D1".into(), None))
        );
        assert_eq!(from("msg_h2:create_task:0a1b2c3d"), None);
        assert_eq!(from("msg_h3:create_task:0a1b2c3d"), None);
        assert_eq!(from("slack:D1:1700.1:1700.1"), None);
        assert_eq!(from("msg_h1:pause_task"), None);
    }

    /// The developer's words Otter on the host said in a task (D-213) are
    /// known by their command id, and shown in the task's own thread — its
    /// own message's, else its latest need's; once said there, what the task
    /// says next goes there.
    #[test]
    fn words_said_for_the_developer_go_to_the_tasks_thread() {
        use MessageRole::{Controller, User};
        let relayed_one = msg("m1", User, Some("msg_h1:send_to_task:ft_a"));
        assert_eq!(relayed(&relayed_one), Some("msg_h1"));
        assert_eq!(
            relayed(&msg("m2", User, Some("msg_h1:create_task:00"))),
            None
        );
        assert_eq!(
            relayed(&msg("m3", Controller, Some("msg_h1:send_to_task:ft_a"))),
            None
        );
        let post = |task: &str, key: &str, ts: &str| Post {
            task: task.into(),
            key: key.into(),
            channel: "D1".into(),
            ts: ts.into(),
            text: String::new(),
            open: false,
            said: vec![],
            shows: None,
        };
        let posts = vec![
            post("ft_a", "decision:1", "1.1"),
            post("ft_b", "started", "1.2"),
            post("ft_a", "started", "1.3"),
            post("ft_a", "blocked:2", "1.4"),
        ];
        assert_eq!(
            thread_of(&posts, "ft_a").map(|i| &posts[i].ts[..]),
            Some("1.3")
        );
        assert_eq!(
            thread_of(&posts[..2], "ft_a").map(|i| &posts[i].ts[..]),
            Some("1.1")
        );
        assert_eq!(thread_of(&posts, "ft_c"), None);
        // Counted as said there: Otter's reply after it goes to that thread.
        let mut heard = Heard::default();
        let there = Origin {
            channel: "D1".into(),
            thread: Some("1.3".into()),
        };
        heard.said.insert("m1".into(), there.clone());
        let conversation = vec![relayed_one, msg("m4", Controller, Some("m1"))];
        assert_eq!(destination(&conversation, 1, &heard), Some(there));
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
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        let c = config(&s).unwrap();
        assert_eq!(
            (c.bot.as_str(), c.app.as_deref()),
            ("xoxb-1", Some("xapp-1"))
        );
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
        // Taking its events from otterhub (D-206), the app token isn't
        // this host's: none is needed, and one set isn't used.
        s.update(otter_protocol::host::SettingsUpdate {
            slack: Some(otter_protocol::host::SlackUpdate {
                hub: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(config(&s).unwrap().app, None);
        s.update(otter_protocol::host::SettingsUpdate {
            secrets: [(SLACK_APP_TOKEN.to_owned(), None)].into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(config(&s).unwrap().bot, "xoxb-1");
        s.update(otter_protocol::host::SettingsUpdate {
            slack: Some(otter_protocol::host::SlackUpdate {
                hub: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(config(&s).err().unwrap(), "Add the app token (xapp-…).");
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
