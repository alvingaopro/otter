//! `otter hub` (D-206): one Slack app for several hosts.
//!
//! Slack shares an app's events among every connection the app has open
//! (D-203), so one may be open. `otter hub serve` holds it for the hosts in
//! `hosts.toml`, and gives each event to the host it is for
//! (`slack.deliver`), which takes it as a connection of its own would and
//! says Otter's messages in Slack itself, as the same bot. Otter, its tools,
//! memory and tasks stay on each host: the hub keeps only whose each message
//! is, and which host each conversation was with last.
//!
//! Where what Slack says goes:
//! - a message in a thread: to the host whose message began it — one the hub
//!   passed on, or one a host says is its own (`slack.owns`);
//! - a message at the top: to the host it begins with (`gpu: …`), else the
//!   one whose workspace it names (D-182), else the one this conversation was
//!   with last, else the default host; else the developer is asked;
//! - a button: to the host whose message it is on;
//! - `/otter status` and `help`: to every host, answered together;
//!   `/otter new`: as a message at the top.
//!
//! ```text
//! $OTTER_CONFIG_DIR/hub/state.json   whose each message is; each conversation's host
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use futures_util::{SinkExt, StreamExt};
use otter_client::{ClientError, Connection, Transport};
use otter_core::{Workspace, WorkspaceState};
use otter_protocol::Request;
use otter_protocol::host::{
    Settings, SettingsUpdate, SlackDeliver, SlackDelivered, SlackOwns, SlackUpdate,
};
use otter_slack::socket::{self, Envelope, Origin, Verb};
use otter_slack::{EnvMap, api};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message as Frame;

use crate::config::{Config, HostEntry};

#[derive(Subcommand)]
pub enum HubCommand {
    /// Hold the Slack app's one connection for your hosts, and give each
    /// what is its own: run it as a service, with `SLACK_BOT_TOKEN` and
    /// `SLACK_APP_TOKEN` in its environment.
    Serve {
        /// A Slack member whose direct messages are the developer's (`U…`;
        /// again for another).
        #[arg(long = "user", required = true)]
        users: Vec<String>,
        /// Only this host (again for another; default: every host).
        #[arg(long = "host")]
        hosts: Vec<String>,
    },
    /// How each host is in Slack: through the hub, on an app of its own, or
    /// not at all.
    Status,
    /// Let a host connect to Slack on its own again (as before `serve`).
    Leave { host: String },
}

/// Slack's tokens for `serve`, read before anything starts.
struct Tokens {
    bot: Option<String>,
    app: Option<String>,
}

static TOKENS: OnceLock<Tokens> = OnceLock::new();

/// Take Slack's tokens out of this process's environment and keep them for
/// `otter hub serve`: nothing started from here — ssh, `otterd dial`, a
/// daemon `otterd dial` starts — inherits them, since secrets never go on a
/// command line or into another process's environment. Called first in
/// `main`, before any thread.
pub fn take_tokens() {
    let take = |name: &str| {
        let value = std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        // SAFETY: no other thread exists yet (`main`, before the runtime),
        // so nothing reads the environment meanwhile.
        unsafe { std::env::remove_var(name) };
        value
    };
    let _ = TOKENS.set(Tokens {
        bot: take("SLACK_BOT_TOKEN"),
        app: take("SLACK_APP_TOKEN"),
    });
}

/// Silence on the socket longer than this is a dead connection (Slack pings
/// far more often).
const QUIET: Duration = Duration::from_secs(150);
/// How long a host gets to take what Slack said.
const DELIVER_WAIT: Duration = Duration::from_secs(30);
/// Between tries to give a host what Slack said, while it can't be reached.
const RETRY: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(6),
    Duration::from_secs(20),
];
/// How long each host gets to say whether a message is its own, or which
/// workspaces it has.
const ASK_WAIT: Duration = Duration::from_secs(3);
/// A host's workspaces, as asked this long ago, still route a message.
const FRESH: Duration = Duration::from_secs(10);
/// `/otter` is answered within Slack's three seconds, or not at all.
const COMMAND_WAIT: Duration = Duration::from_millis(2500);
const HOST_COMMAND_WAIT: Duration = Duration::from_millis(2000);
/// How long a host gets to say and change its settings.
const SETTINGS_WAIT: Duration = Duration::from_secs(15);
/// How often a host that couldn't be reached is tried again.
const ENROLL_AGAIN: Duration = Duration::from_secs(60);
/// Messages whose host is kept, at most: the newest.
const MAX_OWNERS: usize = 5000;
/// Slack's limit for the Markdown in one message.
const MAX_TEXT: usize = 11_000;

pub async fn command(config: &Config, cmd: HubCommand, json: bool) -> Result<()> {
    match cmd {
        HubCommand::Serve { users, hosts } => serve(config, users, hosts).await,
        HubCommand::Status => status(config, json).await,
        HubCommand::Leave { host } => leave(config, &host, json).await,
    }
}

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

/// Why a host didn't do what it was asked.
#[derive(Debug)]
enum Failure {
    /// It said no, and why: trying again won't help.
    Refused(otter_protocol::ErrorCode, String),
    /// It wasn't reached, or didn't answer in time.
    Unreachable(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused(_, why) | Failure::Unreachable(why) => f.write_str(why),
        }
    }
}

/// A host the hub serves, and its connection while it has one.
struct Link {
    name: String,
    transport: Transport,
    conn: tokio::sync::Mutex<Option<Connection>>,
    /// Its workspaces as last asked, and when: what routes a message (D-182).
    workspaces: std::sync::Mutex<Option<(Instant, Vec<Workspace>)>>,
}

impl Link {
    fn new(config: &Config, host: &HostEntry) -> Link {
        let mut transport = config.transport(host);
        // A service can't answer a prompt: a host whose key needs one is
        // unreachable, not waited for.
        if let Transport::Ssh { ssh_args, .. } = &mut transport {
            ssh_args.extend(["-o".to_owned(), "BatchMode=yes".to_owned()]);
        }
        Link {
            name: host.name.clone(),
            transport,
            conn: tokio::sync::Mutex::new(None),
            workspaces: std::sync::Mutex::new(None),
        }
    }

    /// One call within `wait` (waiting for the connection too), connecting
    /// first when there is no connection; one that broke is made again once.
    /// The connection is out of its place while it is used and put back only
    /// once its call is over: a call cut short anyhow — `wait`, or whoever
    /// awaits this dropping it — drops it, since its answers come in order.
    async fn call<T: DeserializeOwned>(
        &self,
        request: Request,
        wait: Duration,
    ) -> Result<T, Failure> {
        let done = tokio::time::timeout(wait, async {
            let mut slot = self.conn.lock().await;
            for again in [false, true] {
                let (mut conn, fresh) = match slot.take() {
                    Some(conn) => (conn, false),
                    None => {
                        let made = Connection::connect(&self.transport).await.map_err(|e| {
                            match e {
                                // Not to be tried again as it is.
                                ClientError::ProtocolMismatch { .. } => Failure::Refused(
                                    otter_protocol::ErrorCode::Unsupported,
                                    format!(
                                        "{e}: update otter or this host (`otter host install`)"
                                    ),
                                ),
                                e => Failure::Unreachable(e.to_string()),
                            }
                        })?;
                        (made, true)
                    }
                };
                match conn.call(request.clone()).await {
                    Ok(v) => {
                        *slot = Some(conn);
                        return Ok(v);
                    }
                    Err(ClientError::Rpc(e)) => {
                        *slot = Some(conn);
                        return Err(Failure::Refused(e.code, e.message));
                    }
                    Err(e) if fresh || again => return Err(Failure::Unreachable(e.to_string())),
                    // Broken since its last call: made again.
                    Err(_) => {}
                }
            }
            unreachable!("the second try returns")
        })
        .await;
        done.unwrap_or_else(|_| {
            Err(Failure::Unreachable(format!(
                "no answer in {}s",
                wait.as_secs_f32()
            )))
        })
    }
}

// ---------------------------------------------------------------------------
// What the hub keeps
// ---------------------------------------------------------------------------

/// Whose each message is, and where each conversation was last
/// (`hub/state.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    /// The host of each message, by `channel:ts`, newest last: the
    /// developer's the hub passed on, and those a host said are its own.
    #[serde(default)]
    owners: VecDeque<(String, String)>,
    /// The host each conversation (a direct message channel) was with last.
    #[serde(default)]
    last: BTreeMap<String, String>,
}

impl Ledger {
    fn load(path: &Path) -> Ledger {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn owner(&self, channel: &str, ts: &str) -> Option<String> {
        let key = format!("{channel}:{ts}");
        self.owners
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, h)| h.clone())
    }

    fn own(&mut self, channel: &str, ts: &str, host: &str) {
        self.owners
            .push_back((format!("{channel}:{ts}"), host.to_owned()));
        while self.owners.len() > MAX_OWNERS {
            self.owners.pop_front();
        }
    }
}

/// Write `bytes` to `path` whole or not at all, readable by its owner only.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

/// Where a message goes.
#[derive(Debug, PartialEq)]
enum Choice {
    /// To `host`: named before the message (`named`, with the words after
    /// the name), or found by `why` (D-182), or neither (where the
    /// conversation was, the default).
    Host {
        host: String,
        why: Option<String>,
        named: Option<String>,
    },
    /// None is the one: the developer chooses among these.
    Ask(Vec<String>),
}

struct Hub {
    links: Vec<Arc<Link>>,
    /// The hosts that take Slack's events from here now.
    enrolled: std::sync::Mutex<BTreeSet<String>>,
    /// Hosts whose otterd doesn't keep the name the hub gives (before
    /// D-207): not given it again.
    unnamed: std::sync::Mutex<BTreeSet<String>>,
    /// Hosts that left (`otter hub leave`) while it served: not taken in
    /// again until it starts again.
    left: std::sync::Mutex<BTreeSet<String>>,
    default_host: Option<String>,
    bot: String,
    bot_id: String,
    users: Vec<String>,
    env: EnvMap,
    ledger: std::sync::Mutex<Ledger>,
    path: PathBuf,
    /// One at a time in each conversation, in the order Slack sent them, as
    /// a host's own connection takes them: a message is placed, and given a
    /// first try, before the next (which may go where it went). By channel,
    /// the last turn taken, done when its sender is.
    turns: std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Receiver<()>>>,
}

async fn serve(config: &Config, users: Vec<String>, only: Vec<String>) -> Result<()> {
    let tokens = TOKENS.get().context("Slack's tokens weren't read")?;
    let bot = tokens
        .bot
        .clone()
        .filter(|t| t.starts_with("xoxb-"))
        .context(
            "set SLACK_BOT_TOKEN (the Slack app's bot token, xoxb-…) in the hub's environment",
        )?;
    let app = tokens
        .app
        .clone()
        .filter(|t| t.starts_with("xapp-"))
        .context("set SLACK_APP_TOKEN (the Slack app's app-level token, xapp-…, connections:write) in the hub's environment")?;
    let users = otter_slack::members(&users).map_err(anyhow::Error::msg)?;
    let hosts: Vec<HostEntry> = match only.is_empty() {
        true => config.hosts.clone(),
        false => only
            .iter()
            .map(|h| config.host(h).cloned())
            .collect::<Result<_>>()?,
    };
    if hosts.is_empty() {
        bail!("no hosts registered; add one with `otter host add <name>`");
    }
    let env: EnvMap = std::env::vars().collect();
    let me = api::call(&env, &bot, "auth.test", &json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("{e} (the bot token)"))?;
    let bot_id = me["bot_id"]
        .as_str()
        .context("Slack didn't say whose bot the token is")?
        .to_owned();
    eprintln!(
        "otter hub: the Slack app's bot `{}` ({bot_id}) in {}",
        me["user"].as_str().unwrap_or("?"),
        me["team"].as_str().unwrap_or("?")
    );
    let dir = Config::dir_from_env()?.join("hub");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let path = dir.join("state.json");
    let hub = Arc::new(Hub {
        links: hosts
            .iter()
            .map(|h| Arc::new(Link::new(config, h)))
            .collect(),
        enrolled: Default::default(),
        unnamed: Default::default(),
        left: Default::default(),
        default_host: config.default_host.clone(),
        bot,
        bot_id,
        users,
        env,
        ledger: std::sync::Mutex::new(Ledger::load(&path)),
        path,
        turns: Default::default(),
    });
    // Every host first, then Slack: a host still connected on its own would
    // hear some of what is said.
    let mut said = HashMap::new();
    hub.enroll_all(&mut said).await;
    let keep = hub.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(ENROLL_AGAIN).await;
            keep.enroll_all(&mut said).await;
        }
    });
    let mut wait = Duration::ZERO;
    loop {
        let began = Instant::now();
        match hub.session(&app).await {
            // Slack closed it: open another.
            Ok(()) => wait = Duration::from_secs(1),
            Err(e) => {
                let why = otter_core::redact::redact(&format!("{e:#}"));
                eprintln!("otter hub: Slack: {why}");
                // A connection that lasted starts the waiting over.
                if began.elapsed() > Duration::from_secs(60) {
                    wait = Duration::ZERO;
                }
                wait = (wait * 2).clamp(Duration::from_secs(2), Duration::from_secs(120));
            }
        }
        tokio::time::sleep(wait).await;
    }
}

impl Hub {
    fn link(&self, host: &str) -> Option<&Arc<Link>> {
        self.links.iter().find(|l| l.name == host)
    }

    /// The hosts served now, by name.
    fn serving(&self) -> Vec<Arc<Link>> {
        let enrolled = self.enrolled.lock().unwrap();
        self.links
            .iter()
            .filter(|l| enrolled.contains(&l.name))
            .cloned()
            .collect()
    }

    /// Keep the ledger: written under its lock, so one write at a time, the
    /// newest last.
    fn save(&self) {
        let ledger = self.ledger.lock().unwrap();
        let bytes = serde_json::to_vec(&*ledger).expect("serializes");
        if let Err(e) = write_private(&self.path, &bytes) {
            eprintln!("otter hub: saving {}: {e}", self.path.display());
        }
    }

    /// Take in each host not served yet: those that can't be reached are
    /// tried again later. What is said of each is said when it changes.
    async fn enroll_all(self: &Arc<Self>, said: &mut HashMap<String, String>) {
        let mut jobs = JoinSet::new();
        for link in self.links.clone() {
            let skip = self.enrolled.lock().unwrap().contains(&link.name)
                || self.left.lock().unwrap().contains(&link.name);
            if skip {
                continue;
            }
            let hub = self.clone();
            jobs.spawn(async move {
                let r = hub.enroll(&link).await;
                (link.name.clone(), r)
            });
        }
        for (host, r) in jobs.join_all().await {
            let line = match &r {
                Ok(line) | Err(line) => line.clone(),
            };
            if r.is_ok() {
                self.enrolled.lock().unwrap().insert(host.clone());
            }
            if said.get(&host) != Some(&line) {
                eprintln!("otter hub: {host}: {line}");
                said.insert(host, line);
            }
        }
    }

    /// Set `link`'s host to take Slack's events from here, with this app's
    /// bot to answer as, when `take_in` says it may; what is said of it.
    async fn enroll(&self, link: &Link) -> Result<String, String> {
        let settings: Settings = link
            .call(Request::SettingsGet, SETTINGS_WAIT)
            .await
            .map_err(|e| match e {
                Failure::Unreachable(e) => format!("not reached ({e}); tried again in a minute"),
                e => e.to_string(),
            })?;
        let Some(slack) = settings.slack else {
            return Err("its otterd is older than Slack: update it (`otter host install`)".into());
        };
        // An otterd before the hub ignores `hub`: asked first, so nothing
        // is set on it.
        let probe = SlackOwns {
            channel: String::new(),
            ts: String::new(),
        };
        if let Err(e) = link
            .call::<bool>(Request::SlackOwns(probe), SETTINGS_WAIT)
            .await
        {
            return Err(match e {
                Failure::Refused(..) => {
                    "its otterd is too old for the hub: update it (`otter host install`)".into()
                }
                Failure::Unreachable(e) => format!("not reached ({e}); tried again in a minute"),
            });
        }
        let give_token = take_in(&slack, &settings.secrets, &self.bot_id)?;
        let mut line = "takes Slack's events from the hub".to_owned();
        // A host that lists none is given the hub's (below).
        let missing: Vec<&str> = self
            .users
            .iter()
            .filter(|u| !slack.users.is_empty() && !slack.users.contains(u))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            line.push_str(&format!(
                " (it answers its own members only: not {})",
                missing.join(", ")
            ));
        }
        // Its messages at the top say which host they are from (D-207).
        let given: String = link
            .name
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        let named = slack.name.as_deref() == Some(given.trim())
            || self.unnamed.lock().unwrap().contains(&link.name);
        if !give_token && !slack.users.is_empty() && named {
            return Ok(line);
        }
        let mut update = SettingsUpdate {
            slack: Some(SlackUpdate {
                // Members the host lists already stay its own.
                users: slack.users.is_empty().then(|| self.users.clone()),
                hub: Some(true),
                // As a host's name may be (one line, 64 characters).
                name: Some(given),
                ..Default::default()
            }),
            ..Default::default()
        };
        // A new token starts its Slack over: given only when needed.
        if give_token {
            update
                .secrets
                .insert("SLACK_BOT_TOKEN".into(), Some(self.bot.clone()));
        }
        let after: Settings = link
            .call(Request::SettingsSet(update), SETTINGS_WAIT)
            .await
            .map_err(|e| format!("not set up: {e}"))?;
        if after.slack.as_ref().is_some_and(|s| s.name.is_none()) {
            self.unnamed.lock().unwrap().insert(link.name.clone());
        }
        match after.slack.and_then(|s| s.hub) {
            Some(true) => Ok(line),
            _ => Err("its otterd is too old for the hub: update it (`otter host install`)".into()),
        }
    }

    /// One connection to Slack, until Slack closes it (`Ok`) or it fails.
    async fn session(self: &Arc<Self>, app: &str) -> Result<()> {
        let open = api::call(&self.env, app, "apps.connections.open", &json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("{e} (the app token)"))?;
        // Its address carries a ticket: never logged.
        let url = open["url"]
            .as_str()
            .context("Slack gave no address to connect to")?;
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| anyhow::anyhow!("connecting to Slack: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        // Acks go apart from reading, so `/otter` being answered holds
        // nothing up.
        let (acks, mut out) = tokio::sync::mpsc::unbounded_channel::<String>();
        let write = async {
            while let Some(text) = out.recv().await {
                sink.send(Frame::text(text)).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let read = async {
            let ack = |id: &str| {
                let _ = acks.send(json!({ "envelope_id": id }).to_string());
            };
            let quiet = tokio::time::sleep(QUIET);
            tokio::pin!(quiet);
            loop {
                tokio::select! {
                    () = &mut quiet => bail!("Slack went quiet"),
                    frame = stream.next() => {
                        quiet.as_mut().reset(tokio::time::Instant::now() + QUIET);
                        let text = match frame {
                            None | Some(Ok(Frame::Close(_))) => return Ok(()),
                            Some(Err(e)) => bail!("the connection to Slack: {e}"),
                            Some(Ok(Frame::Text(t))) => t,
                            Some(Ok(_)) => continue,
                        };
                        let raw: Value = serde_json::from_str(text.as_str()).unwrap_or_default();
                        match socket::envelope(text.as_str()) {
                            Envelope::Hello => eprintln!("otter hub: connected to Slack"),
                            Envelope::Disconnect(why) => {
                                eprintln!("otter hub: Slack closed the connection ({why})");
                                return Ok(());
                            }
                            // Its turn taken here, in Slack's order: what is
                            // given waits for it, not the reading.
                            Envelope::Event { envelope_id, event } => {
                                ack(&envelope_id);
                                let turn = self.turn(event["channel"].as_str().unwrap_or_default());
                                tokio::spawn(self.clone().heard(raw, turn));
                            }
                            Envelope::Interactive { envelope_id, payload } => {
                                ack(&envelope_id);
                                let channel = payload["container"]["channel_id"]
                                    .as_str()
                                    .or_else(|| payload["channel"]["id"].as_str())
                                    .unwrap_or_default();
                                let turn = self.turn(channel);
                                tokio::spawn(self.clone().pressed(raw, turn));
                            }
                            Envelope::Command { envelope_id, payload } => {
                                let (hub, acks) = (self.clone(), acks.clone());
                                // Not "ask again" for a task that may be
                                // starting: typed again, it is another.
                                let late = match socket::command(&payload).map(|c| c.verb) {
                                    Some(Verb::New(_)) => "Starting it: its thread will be in your direct messages with Otter.",
                                    _ => "Otter is busy; ask again in a moment.",
                                };
                                tokio::spawn(async move {
                                    let payload = tokio::time::timeout(COMMAND_WAIT, hub.command(raw))
                                        .await
                                        .unwrap_or_else(|_| answer(late));
                                    let _ = acks.send(
                                        json!({ "envelope_id": envelope_id, "payload": payload }).to_string(),
                                    );
                                });
                            }
                            Envelope::Other { envelope_id } => ack(&envelope_id),
                            Envelope::Unknown => {}
                        }
                    }
                }
            }
        };
        tokio::select! {
            r = read => r,
            r = write => r,
        }
    }

    /// A word of the hub's own in Slack, where `to` is.
    async fn post(&self, to: &Origin, text: &str) {
        let mut body = json!({
            "channel": to.channel,
            "text": text.chars().take(280).collect::<String>(),
            "blocks": [{ "type": "markdown", "text": text }],
            "unfurl_links": false,
            "unfurl_media": false,
        });
        if let Some(t) = &to.thread {
            body["thread_ts"] = json!(t);
        }
        if let Err(e) = api::call(&self.env, &self.bot, "chat.postMessage", &body).await {
            eprintln!("otter hub: saying in Slack: {e}");
        }
    }

    /// The host whose message `ts` in `channel` is: as kept, else as a host
    /// says (every host asked at once).
    async fn owner(&self, channel: &str, ts: &str) -> Option<String> {
        let serving = self.serving();
        // Kept: its host's, reachable or not (then the developer is told).
        let kept = self.ledger.lock().unwrap().owner(channel, ts);
        if let Some(host) = kept.filter(|h| self.link(h).is_some()) {
            return Some(host);
        }
        let mut jobs = JoinSet::new();
        for link in serving {
            let ask = SlackOwns {
                channel: channel.to_owned(),
                ts: ts.to_owned(),
            };
            jobs.spawn(async move {
                let owns = link
                    .call::<bool>(Request::SlackOwns(ask), ASK_WAIT)
                    .await
                    .unwrap_or(false);
                (link.name.clone(), owns)
            });
        }
        let host = jobs
            .join_all()
            .await
            .into_iter()
            .find_map(|(host, owns)| owns.then_some(host))?;
        self.ledger.lock().unwrap().own(channel, ts, &host);
        self.save();
        Some(host)
    }

    /// Each host's workspaces, as routing takes them: asked again when older
    /// than `FRESH` and `ask`, else as last asked. Put away (D-105) is not
    /// where a message is about.
    async fn workspaces(&self, ask: bool) -> Vec<(String, Vec<Workspace>)> {
        let mut jobs = JoinSet::new();
        for link in self.serving() {
            jobs.spawn(async move {
                let kept = link.workspaces.lock().unwrap().clone();
                let stale = kept.as_ref().is_none_or(|(at, _)| at.elapsed() > FRESH);
                let list = match (ask && stale, kept) {
                    (false, kept) => kept.map(|(_, l)| l),
                    (true, kept) => {
                        match link
                            .call::<Vec<Workspace>>(Request::WorkspaceList, ASK_WAIT)
                            .await
                        {
                            Ok(list) => {
                                *link.workspaces.lock().unwrap() =
                                    Some((Instant::now(), list.clone()));
                                Some(list)
                            }
                            Err(_) => kept.map(|(_, l)| l),
                        }
                    }
                };
                (link.name.clone(), list)
            });
        }
        let mut out: Vec<(String, Vec<Workspace>)> = jobs
            .join_all()
            .await
            .into_iter()
            .filter_map(|(host, list)| {
                let mut list = list?;
                list.retain(|w| w.state != WorkspaceState::Archived);
                Some((host, list))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The host a message at the top of `channel` is for.
    async fn choose(&self, channel: &str, text: &str, ask: bool) -> Choice {
        let serving: Vec<String> = self.serving().iter().map(|l| l.name.clone()).collect();
        if let Some((host, rest)) = named(text, &serving) {
            return Choice::Host {
                host,
                why: None,
                named: Some(rest),
            };
        }
        let last = self.ledger.lock().unwrap().last.get(channel).cloned();
        let routed = otter_client::route::route(text, &self.workspaces(ask).await);
        decide(routed, &serving, last, self.default_host.clone())
    }

    /// The next turn in `channel`'s conversation (`turns`).
    fn turn(&self, channel: &str) -> Turn {
        Turn::next(&self.turns, channel)
    }

    /// Give `host` what Slack said, trying again a while if it can't be
    /// reached; the developer is told, at `to`, when it can't be given.
    /// `turn` is held until the first try is over: what comes next in the
    /// conversation waits for it, not for the tries after.
    async fn deliver(&self, host: &str, envelope: &Value, to: &Origin, mut turn: Option<Turn>) {
        let Some(link) = self.link(host) else {
            return;
        };
        let give = || {
            Request::SlackDeliver(SlackDeliver {
                envelope: envelope.clone(),
            })
        };
        let mut tries = RETRY.iter();
        loop {
            match link.call::<SlackDelivered>(give(), DELIVER_WAIT).await {
                Ok(_) => return,
                Err(Failure::Refused(code, why)) => {
                    eprintln!("otter hub: {host} didn't take a Slack event: {why}");
                    if code == otter_protocol::ErrorCode::Conflict {
                        // Left (`otter hub leave`): not served again until
                        // the hub starts again.
                        self.enrolled.lock().unwrap().remove(host);
                        self.left.lock().unwrap().insert(host.to_owned());
                    }
                    self.post(to, &format!("**{host}** didn't take that: {why}"))
                        .await;
                    return;
                }
                Err(Failure::Unreachable(why)) => {
                    turn.take();
                    let Some(wait) = tries.next() else {
                        eprintln!("otter hub: {host}: {why}; a Slack event wasn't given");
                        break;
                    };
                    eprintln!("otter hub: {host}: {why}; trying again");
                    tokio::time::sleep(*wait).await;
                }
            }
        }
        self.post(
            to,
            &format!("**{host}** isn't reachable now, so that wasn't passed on. Say it again later, or begin it with another host's name (`name: …`)."),
        )
        .await;
    }

    /// An event: the developer's direct message, given to its host.
    async fn heard(self: Arc<Self>, mut envelope: Value, mut turn: Turn) {
        // First: one not taken (the bot's own messages among them) mustn't
        // let the next pass the one before it.
        turn.wait().await;
        let m = match socket::inbound(&envelope["payload"]["event"], &self.users) {
            Ok(m) => m,
            Err(socket::Skip::NotListed) => {
                eprintln!("otter hub: a message from a Slack member the hub doesn't list");
                return;
            }
            Err(_) => return,
        };
        let channel = m.to.channel.clone();
        let host = match &m.to.thread {
            // A reply goes where its thread began; one under a message no
            // host knows, where the conversation was.
            Some(thread) => {
                let last = self.ledger.lock().unwrap().last.get(&channel).cloned();
                let found = self.owner(&channel, thread).await;
                match found.or(last).or_else(|| self.only_or_default()) {
                    Some(h) => h,
                    None => return self.ask(&m.to, &self.names()).await,
                }
            }
            None => match self.choose(&channel, &m.text, true).await {
                Choice::Ask(hosts) => return self.ask(&m.to, &hosts).await,
                Choice::Host { host, why, named } => {
                    let before = self
                        .ledger
                        .lock()
                        .unwrap()
                        .last
                        .insert(channel.clone(), host.clone());
                    self.save();
                    if let Some(rest) = &named {
                        if rest.is_empty() && m.files.is_empty() {
                            self.post(&m.to, &format!("Talking to **{host}** now."))
                                .await;
                            return;
                        }
                        // The host hears the message without its name.
                        let raw = envelope["payload"]["event"]["text"]
                            .as_str()
                            .unwrap_or_default();
                        let rest = named_in(raw, &host).unwrap_or_else(|| rest.clone());
                        envelope["payload"]["event"]["text"] = json!(rest);
                    } else if before.as_deref() != Some(host.as_str()) && self.serving().len() > 1 {
                        // A change of host is said, as `otter chat` says it.
                        let why = why.map(|w| format!(" ({w})")).unwrap_or_default();
                        self.post(&m.to, &format!("→ **{host}**{why}")).await;
                    }
                    host
                }
            },
        };
        self.ledger.lock().unwrap().own(&channel, &m.ts, &host);
        self.save();
        self.deliver(&host, &envelope, &m.to, Some(turn)).await;
    }

    /// The only host served, else the default one.
    fn only_or_default(&self) -> Option<String> {
        let names = self.names();
        match names.as_slice() {
            [only] => Some(only.clone()),
            _ => self.default_host.clone().filter(|d| names.contains(d)),
        }
    }

    fn names(&self) -> Vec<String> {
        self.serving().iter().map(|l| l.name.clone()).collect()
    }

    /// Ask which host a message is for.
    async fn ask(&self, to: &Origin, hosts: &[String]) {
        let line = match hosts {
            [] => "No host takes Slack from the hub now (`otter hub status` says why): that wasn't passed on.".to_owned(),
            _ => format!(
                "Which host is that for? Say it again beginning with its name: {}.",
                hosts
                    .iter()
                    .map(|h| format!("`{h}: …`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        self.post(to, &line).await;
    }

    /// A button pressed on a host's message, given to that host.
    async fn pressed(self: Arc<Self>, envelope: Value, mut turn: Turn) {
        turn.wait().await;
        let Some(p) = socket::press(&envelope["payload"]) else {
            return;
        };
        if !self.users.contains(&p.user) {
            eprintln!("otter hub: a button pressed by a Slack member the hub doesn't list");
            return;
        }
        let ts = p.on.thread.clone().unwrap_or_default();
        match self.owner(&p.on.channel, &ts).await {
            Some(host) => self.deliver(&host, &envelope, &p.on, Some(turn)).await,
            None => {
                self.post(
                    &p.on,
                    "The host this message is from isn't reachable now: press again later.",
                )
                .await
            }
        }
    }

    /// `/otter`: what needs the developer on every host (`status`, `help`),
    /// or a task started on the one its words are for (`new`). Its answer
    /// is the ack's.
    async fn command(self: Arc<Self>, mut envelope: Value) -> Value {
        let Some(cmd) = socket::command(&envelope["payload"]) else {
            return answer("Otter only knows `/otter`.");
        };
        if !self.users.contains(&cmd.user) {
            return answer("This Otter answers only the developer it is set up for.");
        }
        let serving = self.serving();
        if serving.is_empty() {
            return answer("No host takes Slack from the hub now: `otter hub status` says why.");
        }
        let request = match cmd.verb {
            Verb::New(request) => request,
            Verb::Status | Verb::Help => {
                let mut jobs = JoinSet::new();
                for link in serving {
                    let give = Request::SlackDeliver(SlackDeliver {
                        envelope: envelope.clone(),
                    });
                    jobs.spawn(async move {
                        let r = link.call::<SlackDelivered>(give, HOST_COMMAND_WAIT).await;
                        (link.name.clone(), r)
                    });
                }
                let mut said = jobs.join_all().await;
                said.sort_by(|a, b| a.0.cmp(&b.0));
                for (host, r) in &said {
                    if let Err(Failure::Refused(otter_protocol::ErrorCode::Conflict, _)) = r {
                        self.enrolled.lock().unwrap().remove(host);
                        self.left.lock().unwrap().insert(host.clone());
                    }
                }
                return together(said);
            }
        };
        let channel = envelope["payload"]["channel_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        // Answered within Slack's three seconds: routed by what the hosts
        // said last, not asked again.
        let host = match self.choose(&channel, &request, false).await {
            Choice::Ask(hosts) => {
                return answer(&format!(
                    "Which host is that for? `/otter new <host>: …`, with one of {}.",
                    hosts.join(", ")
                ));
            }
            Choice::Host { host, named, .. } => {
                if let Some(rest) = named {
                    envelope["payload"]["text"] = json!(format!("new {rest}"));
                }
                host
            }
        };
        self.ledger
            .lock()
            .unwrap()
            .last
            .insert(channel, host.clone());
        self.save();
        // Given whatever comes of the ack, never cut short: a task the host
        // may be making then is not asked for again (D-202 keeps one typing
        // of `/otter new` to one task; a second typing is another).
        let link = self.link(&host).expect("served").clone();
        let give = Request::SlackDeliver(SlackDeliver { envelope });
        let mut given =
            tokio::spawn(async move { link.call::<SlackDelivered>(give, DELIVER_WAIT).await });
        match tokio::time::timeout(HOST_COMMAND_WAIT, &mut given).await {
            Ok(Ok(Ok(d))) => d.ack.unwrap_or_else(|| answer("")),
            Ok(Ok(Err(e))) => answer(&format!("**{host}** didn't take that: {e}")),
            Ok(Err(_)) => answer(&format!("**{host}** didn't take that.")),
            Err(_) => {
                // What comes of it later is said where Slack said to.
                let (env, url, at) = (self.env.clone(), cmd.respond.clone(), host.clone());
                tokio::spawn(async move {
                    if let (Ok(Err(e)), Some(url)) = (given.await, url) {
                        let line = format!("**{at}** didn't take that: {e}");
                        if let Err(e) = api::respond(&env, &url, &line).await {
                            eprintln!("otter hub: {e}");
                        }
                    }
                });
                answer(&format!(
                    "Starting it on **{host}**: its thread will be in your direct messages with Otter."
                ))
            }
        }
    }
}

/// A place in a conversation's line (`Hub::turns`): the one before it is
/// waited for, and the one after waits until this is dropped.
struct Turn {
    after: Option<tokio::sync::oneshot::Receiver<()>>,
    _done: tokio::sync::oneshot::Sender<()>,
}

impl Turn {
    /// The next place in `channel`'s line, taken now.
    fn next(
        turns: &std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Receiver<()>>>,
        channel: &str,
    ) -> Turn {
        let (done, next) = tokio::sync::oneshot::channel();
        let after = turns.lock().unwrap().insert(channel.to_owned(), next);
        Turn { after, _done: done }
    }

    /// Until the turn before is over (sent or dropped); cut short, still
    /// waiting for it the next time.
    async fn wait(&mut self) {
        if let Some(before) = &mut self.after {
            let _ = before.await;
            self.after = None;
        }
    }
}

/// Whether a host may be taken in by the hub on bot `bot_id`, and whether
/// it needs the bot's token: when nothing of Slack is set on it, or it is on
/// this app already (its own connection must stop, or it would hear part of
/// what is said), or it takes its events from a hub on this app. A host on
/// another app, or with tokens of an app the hub can't tell is this one, is
/// left as it is; why, if so.
fn take_in(
    slack: &otter_protocol::host::SlackView,
    secrets: &[otter_protocol::host::SecretState],
    bot_id: &str,
) -> Result<bool, String> {
    let set = |name: &str| secrets.iter().any(|k| k.name == name && k.set);
    let hubbed = slack.hub == Some(true);
    match slack.bot_id.as_deref() {
        Some(b) if b == bot_id => Ok(!hubbed),
        Some(b) if hubbed => Err(format!(
            "left as it is: it takes its events from a hub on another Slack app (bot {b})"
        )),
        Some(b) => Err(format!(
            "left as it is: it is on a Slack app of its own (bot {b})"
        )),
        // Not connected: which app is known only by what is set. Nothing
        // set (through a hub or not): nothing to overwrite.
        None if !set("SLACK_BOT_TOKEN") && !set("SLACK_APP_TOKEN") => Ok(true),
        // Through a hub, its bot not checked with Slack yet: it may be
        // another hub's, so asked again once it is.
        None if hubbed && slack.state != "failed" => Err(format!(
            "it takes its events from a hub, but hasn't checked its bot with Slack yet (`{}`); tried again in a minute",
            slack.state
        )),
        None if hubbed => Err(
            "left as it is: it takes its events from a hub, but its bot token doesn't work there; clear its Slack tokens, or `otter hub leave` it, to have it taken in".into(),
        ),
        None => Err(format!(
            "left as it is: Slack is set up there (`{}`) for an app the hub can't tell is this one; clear its Slack tokens to have it taken in, or connect it",
            slack.state
        )),
    }
}

/// Where a message at the top goes, from what it names (D-182), where its
/// conversation was `last`, and the default host, among the hosts
/// `serving`: one that a tie includes, or else the developer is asked.
fn decide(
    routed: otter_client::route::Route,
    serving: &[String],
    last: Option<String>,
    default: Option<String>,
) -> Choice {
    use otter_client::route::Route;
    let host = |h: String| Choice::Host {
        host: h,
        why: None,
        named: None,
    };
    let last = last.filter(|h| serving.contains(h));
    let default = default.filter(|h| serving.contains(h));
    match routed {
        Route::One(m) => Choice::Host {
            host: m.host,
            why: Some(m.why),
            named: None,
        },
        Route::Several(tied) => {
            let tied: Vec<String> = tied.into_iter().map(|m| m.host).collect();
            last.filter(|h| tied.contains(h))
                .or(default.filter(|h| tied.contains(h)))
                .map(host)
                .unwrap_or(Choice::Ask(tied))
        }
        Route::Nowhere => last
            .or(default)
            .or_else(|| match serving {
                [only] => Some(only.clone()),
                _ => None,
            })
            .map(host)
            .unwrap_or_else(|| Choice::Ask(serving.to_vec())),
    }
}

/// The host a message begins with (`gpu: …`, `gpu：…`, `@gpu …`), and the
/// words after it.
fn named(text: &str, hosts: &[String]) -> Option<(String, String)> {
    // The longest name first: `gpu-2` before `gpu`.
    let mut hosts: Vec<&String> = hosts.iter().collect();
    hosts.sort_by_key(|h| std::cmp::Reverse(h.len()));
    hosts
        .into_iter()
        .find_map(|h| named_in(text, h).map(|rest| (h.clone(), rest)))
}

/// The words after `host`'s name at the start of `text`, if it begins so.
fn named_in(text: &str, host: &str) -> Option<String> {
    let t = text.trim_start();
    let (at, t) = match t.strip_prefix('@') {
        Some(t) => (true, t),
        None => (false, t),
    };
    let head = t.get(..host.len())?;
    if !head.eq_ignore_ascii_case(host) {
        return None;
    }
    let rest = &t[host.len()..];
    if let Some(r) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('：')) {
        return Some(r.trim().to_owned());
    }
    (at && rest.chars().next().is_none_or(char::is_whitespace)).then(|| rest.trim().to_owned())
}

/// An answer to `/otter`, seen only by who typed it.
fn answer(text: &str) -> Value {
    let text: String = text.chars().take(MAX_TEXT).collect();
    json!({
        "response_type": "ephemeral",
        "text": text.chars().take(280).collect::<String>(),
        "blocks": [{ "type": "markdown", "text": text }],
    })
}

/// Each host's answer to `/otter`, under its name.
fn together(said: Vec<(String, Result<SlackDelivered, Failure>)>) -> Value {
    let parts: Vec<String> = said
        .into_iter()
        .map(|(host, r)| {
            let words = match r {
                Ok(d) => d
                    .ack
                    .as_ref()
                    .and_then(|a| a["blocks"][0]["text"].as_str().or(a["text"].as_str()))
                    .unwrap_or_default()
                    .to_owned(),
                Err(Failure::Unreachable(e)) => format!("_Not reached: {e}_"),
                Err(Failure::Refused(_, e)) => format!("_Didn't answer: {e}_"),
            };
            format!("**{host}**\n{words}")
        })
        .collect();
    answer(&parts.join("\n\n"))
}

// ---------------------------------------------------------------------------
// Status, and leaving
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HostSlack {
    host: String,
    /// `hub`, `connected` (an app of its own), `off`, `failed`, …; `null`:
    /// not reached.
    state: Option<String>,
    detail: Option<String>,
    bot_id: Option<String>,
    hub: bool,
}

async fn status(config: &Config, json: bool) -> Result<()> {
    if config.hosts.is_empty() {
        bail!("no hosts registered; add one with `otter host add <name>`");
    }
    let mut jobs = JoinSet::new();
    for host in config.hosts.clone() {
        let link = Link::new(config, &host);
        jobs.spawn(async move {
            let r = link
                .call::<Settings>(Request::SettingsGet, SETTINGS_WAIT)
                .await;
            (host.name, r)
        });
    }
    let mut rows: Vec<HostSlack> = jobs
        .join_all()
        .await
        .into_iter()
        .map(|(host, r)| match r.map(|s| s.slack) {
            Ok(Some(s)) => HostSlack {
                host,
                state: Some(s.state),
                detail: s.detail,
                bot_id: s.bot_id,
                hub: s.hub == Some(true),
            },
            Ok(None) => HostSlack {
                host,
                state: None,
                detail: Some("its otterd is older than Slack".into()),
                bot_id: None,
                hub: false,
            },
            Err(e) => HostSlack {
                host,
                state: None,
                detail: Some(format!("not reached: {e}")),
                bot_id: None,
                hub: false,
            },
        })
        .collect();
    rows.sort_by(|a, b| a.host.cmp(&b.host));
    if json {
        return crate::output::print_json(&rows);
    }
    crate::output::table(
        &["HOST", "SLACK", "BOT", "DETAIL"],
        rows.into_iter()
            .map(|r| {
                let slack = match (r.hub, r.state.as_deref()) {
                    (true, Some(s)) => format!("through the hub ({s})"),
                    (false, Some("connected")) => "an app of its own".into(),
                    (false, Some(s)) => s.to_owned(),
                    (_, None) => "-".into(),
                };
                vec![
                    r.host,
                    slack,
                    r.bot_id.unwrap_or_else(|| "-".into()),
                    r.detail.unwrap_or_default(),
                ]
            })
            .collect(),
    );
    Ok(())
}

async fn leave(config: &Config, host: &str, json: bool) -> Result<()> {
    let entry = config.host(host)?;
    let mut conn = crate::target::connect(config, entry).await?;
    let settings = conn
        .settings_set(SettingsUpdate {
            slack: Some(SlackUpdate {
                hub: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await?;
    let slack = settings.slack.context("its otterd is older than Slack")?;
    if json {
        return crate::output::print_json(&slack);
    }
    println!(
        "{host}: Slack's events no longer come from the hub; on its own now it is `{}`{}.",
        slack.state,
        slack.detail.map(|d| format!(" ({d})")).unwrap_or_default()
    );
    println!(
        "Stop a hub serving it first, or start it again with `--host` leaving {host} out: until the hub next gives {host} an event, both are connected as the app, and Slack splits its events between them."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_client::route::Route;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A message beginning with a host's name is for it, without the name;
    /// a name inside the words, or another word before a colon, is not.
    #[test]
    fn a_message_beginning_with_a_hosts_name_is_for_it() {
        let hosts = names(&["gpu", "gpu-2", "mac"]);
        let n = |t: &str| named(t, &hosts);
        assert_eq!(n("gpu: train it"), Some(("gpu".into(), "train it".into())));
        assert_eq!(n("GPU-2:train"), Some(("gpu-2".into(), "train".into())));
        assert_eq!(n("mac：看看"), Some(("mac".into(), "看看".into())));
        assert_eq!(
            n("@mac what's up"),
            Some(("mac".into(), "what's up".into()))
        );
        assert_eq!(n("@mac"), Some(("mac".into(), String::new())));
        assert_eq!(n("gpu:"), Some(("gpu".into(), String::new())));
        assert_eq!(n("gpus: are slow"), None);
        assert_eq!(n("mac what's up"), None);
        assert_eq!(n("on gpu: train"), None);
        assert_eq!(n("Q: why"), None);
    }

    /// At the top, a message goes to the host whose workspace it names;
    /// else where its conversation was; else the default; else the only
    /// host. A tie goes to whichever of those it includes, or is asked.
    #[test]
    fn a_message_goes_where_it_names_else_where_it_was() {
        let serving = names(&["a", "b", "c"]);
        let one = |h: &str| {
            otter_client::route::route(
                &format!("in {h}work do it"),
                &[(h.to_owned(), vec![workspace(&format!("{h}work"))])],
            )
        };
        let host = |h: &str| Choice::Host {
            host: h.into(),
            why: None,
            named: None,
        };
        match decide(one("b"), &serving, Some("a".into()), Some("c".into())) {
            Choice::Host { host, why, .. } => {
                assert_eq!(host, "b");
                assert!(why.unwrap().contains("bwork"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            decide(Route::Nowhere, &serving, Some("a".into()), Some("c".into())),
            host("a")
        );
        assert_eq!(
            decide(Route::Nowhere, &serving, None, Some("c".into())),
            host("c")
        );
        // A host no longer served isn't where it goes.
        assert_eq!(
            decide(Route::Nowhere, &serving, Some("gone".into()), None),
            Choice::Ask(serving.clone())
        );
        assert_eq!(
            decide(Route::Nowhere, &names(&["a"]), None, None),
            host("a")
        );
        let tie = otter_client::route::route(
            "fix billing",
            &[
                ("a".to_owned(), vec![workspace("billing")]),
                ("b".to_owned(), vec![workspace("billing")]),
            ],
        );
        assert!(matches!(tie, Route::Several(_)));
        assert_eq!(
            decide(tie.clone(), &serving, Some("b".into()), Some("a".into())),
            host("b")
        );
        assert_eq!(
            decide(tie.clone(), &serving, Some("c".into()), Some("a".into())),
            host("a")
        );
        assert_eq!(
            decide(tie, &serving, Some("c".into()), None),
            Choice::Ask(names(&["a", "b"]))
        );
    }

    fn workspace(name: &str) -> Workspace {
        let now = chrono::Utc::now();
        Workspace {
            id: format!("ws_{name}").as_str().into(),
            name: name.into(),
            root: format!("/w/{name}"),
            brief: Default::default(),
            source: otter_core::WorkspaceSource::Empty,
            environment: Default::default(),
            state: WorkspaceState::Ready,
            state_message: None,
            created_at: now,
            updated_at: now,
            sessions: vec![],
            attention: vec![],
            task_id: None,
            kept: None,
        }
    }

    /// The hub takes in a host with nothing of Slack set, one on its own
    /// app already, or one through a hub on it; never one on another app,
    /// nor one with tokens of an app it can't tell is its own — that one
    /// isn't connected, so `off` is no proof that nothing is set (D-206).
    #[test]
    fn a_host_on_another_app_is_left_as_it_is() {
        use otter_protocol::host::{SecretState, SlackView};
        let view = |state: &str, bot_id: Option<&str>, hub: bool| SlackView {
            state: state.into(),
            bot_id: bot_id.map(String::from),
            hub: hub.then_some(true),
            ..Default::default()
        };
        let secrets = |bot: bool, app: bool| {
            vec![
                SecretState {
                    name: "SLACK_BOT_TOKEN".into(),
                    purpose: String::new(),
                    set: bot,
                },
                SecretState {
                    name: "SLACK_APP_TOKEN".into(),
                    purpose: String::new(),
                    set: app,
                },
            ]
        };
        let none = secrets(false, false);
        // Nothing set: taken in, with the bot's token.
        assert_eq!(take_in(&view("off", None, false), &none, "B1"), Ok(true));
        // On this app with a connection of its own: taken in, its own stops.
        assert_eq!(
            take_in(
                &view("connected", Some("B1"), false),
                &secrets(true, true),
                "B1"
            ),
            Ok(true)
        );
        // Through the hub on this app already: nothing to give.
        assert_eq!(
            take_in(&view("hub", Some("B1"), true), &secrets(true, false), "B1"),
            Ok(false)
        );
        // Through a hub, not checked with Slack yet: whose is unknown, so
        // asked again later; its token refused there, it is said what to do.
        let why =
            take_in(&view("connecting", None, true), &secrets(true, false), "B1").unwrap_err();
        assert!(why.contains("tried again"), "{why}");
        let why = take_in(&view("failed", None, true), &secrets(true, false), "B1").unwrap_err();
        assert!(why.contains("otter hub leave"), "{why}");
        // Through a hub with its tokens cleared: nothing to overwrite.
        assert_eq!(take_in(&view("off", None, true), &none, "B1"), Ok(true));
        // Another app, connected, or through another hub: left.
        for v in [
            view("connected", Some("B2"), false),
            view("hub", Some("B2"), true),
        ] {
            let why = take_in(&v, &secrets(true, true), "B1").unwrap_err();
            assert!(why.contains("B2"), "{why}");
        }
        // Tokens set, not connected (a member ID missing, a wrong app
        // token): which app is unknown, so left.
        for set in [
            secrets(true, false),
            secrets(false, true),
            secrets(true, true),
        ] {
            for state in ["off", "failed", "connecting"] {
                let why = take_in(&view(state, None, false), &set, "B1").unwrap_err();
                assert!(why.contains("clear its Slack tokens"), "{why}");
            }
        }
    }

    /// A conversation's turns go in the order they were taken: each waits
    /// for the one before it to be over, whatever runs first; another
    /// conversation's don't wait.
    #[tokio::test]
    async fn turns_go_in_the_order_they_were_taken() {
        let turns = Default::default();
        let (mut first, mut second, mut third) = (
            Turn::next(&turns, "D1"),
            Turn::next(&turns, "D1"),
            Turn::next(&turns, "D1"),
        );
        let mut other = Turn::next(&turns, "D2");
        tokio::time::timeout(Duration::from_millis(50), other.wait())
            .await
            .expect("another conversation's doesn't wait");
        first.wait().await;
        let third = tokio::spawn(async move { third.wait().await });
        let short = Duration::from_millis(50);
        assert!(tokio::time::timeout(short, second.wait()).await.is_err());
        // Cut short, it is still waiting.
        assert!(tokio::time::timeout(short, second.wait()).await.is_err());
        drop(first);
        tokio::time::timeout(short, second.wait())
            .await
            .expect("second, once first is over");
        assert!(!third.is_finished());
        drop(second);
        tokio::time::timeout(short, third).await.unwrap().unwrap();
    }

    /// What the hub keeps of whose each message is: the newest says, and
    /// only so many are kept.
    #[test]
    fn whose_each_message_is_is_kept_newest_first() {
        let mut l = Ledger::default();
        l.own("D1", "1.0", "a");
        l.own("D1", "1.0", "b");
        l.own("D2", "1.0", "c");
        assert_eq!(l.owner("D1", "1.0").as_deref(), Some("b"));
        assert_eq!(l.owner("D2", "1.0").as_deref(), Some("c"));
        assert_eq!(l.owner("D1", "2.0"), None);
        for i in 0..MAX_OWNERS {
            l.own("D3", &i.to_string(), "a");
        }
        assert_eq!(l.owners.len(), MAX_OWNERS);
        assert_eq!(l.owner("D1", "1.0"), None);
    }

    /// `/otter status` from several hosts: each under its name, one that
    /// wasn't reached said so.
    #[test]
    fn each_hosts_answer_is_said_under_its_name() {
        let a = SlackDelivered {
            ack: Some(answer("Nothing needs you.")),
        };
        let v = together(vec![
            ("a".into(), Ok(a)),
            (
                "b".into(),
                Err(Failure::Unreachable("no answer in 2s".into())),
            ),
        ]);
        let text = v["blocks"][0]["text"].as_str().unwrap();
        assert_eq!(
            text,
            "**a**\nNothing needs you.\n\n**b**\n_Not reached: no answer in 2s_"
        );
        assert_eq!(v["response_type"], "ephemeral");
    }
}
