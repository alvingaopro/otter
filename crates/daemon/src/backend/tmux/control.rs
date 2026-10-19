//! tmux's side of a stream attach (D-118): a control-mode client (`tmux -C
//! attach-session`) instead of a terminal client. tmux then sends the pane's
//! own output (`%output`), which the client's terminal interprets itself, so
//! the client keeps the history and scrolls it without asking the host.
//!
//! The client's terminal is rebuilt from a snapshot on attach, and again when
//! it falls too far behind, comes back on screen after missing output, or
//! asks for more history (D-119). One command line takes it: tmux runs a
//! line's commands without reading the pane in between, so output that came
//! before the snapshot's replies is in it and output after them is new. Each
//! command gets one reply, and an error drops the rest of its line, so
//! replies are matched to the lines sent by counting.
//!
//! Input goes back with `send-keys -H`, byte for byte. A bracketed paste goes
//! through a paste buffer instead, so tmux, which knows whether the program
//! asked for bracketed paste, adds the brackets or not. The client's terminal
//! can't be told that on attach, so every snapshot turns bracketing on.
//!
//! The reader keeps what the client is: its size, whether it's on screen,
//! the history it keeps. It asks the writer for snapshots and resizes, and
//! tells the client when another client sizes the window.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{EXIT_MARKER, HISTORY_LIMIT, OLD_EXIT_MARKER};
use crate::backend::{Flow, Stream, StreamInput, StreamOutput};

/// Starts the reply that carries a snapshot's state.
const MARKER: &str = "otter-snapshot";
/// The pane's state, in the order [`State::parse`] reads it. Commas keep an
/// empty field (a format this tmux doesn't have) in its place.
const STATE_FORMAT: &str = "#{pane_id},#{pane_width},#{pane_height},#{history_size},\
    #{alternate_on},#{alternate_saved_x},#{alternate_saved_y},#{cursor_x},#{cursor_y},\
    #{cursor_flag},#{keypad_cursor_flag},#{keypad_flag},#{wrap_flag},#{insert_flag},\
    #{origin_flag},#{scroll_region_upper},#{scroll_region_lower},#{mouse_standard_flag},\
    #{mouse_button_flag},#{mouse_all_flag},#{mouse_utf8_flag},#{mouse_sgr_flag},\
    #{cursor_shape},#{cursor_blinking},#{cursor_colour}";
/// Commands in a snapshot's line (see [`snapshot_line`]), and in a resize's.
const SNAPSHOT_REPLIES: usize = 5;
const RESIZE_REPLIES: usize = 2;
/// Output waiting for the client beyond this is dropped for a fresh snapshot.
/// Snapshots don't count.
const BEHIND: usize = 2 << 20;
/// Snapshots after falling behind come at least this far apart, so a client
/// slower than the program sees a screen, not a flicker of them.
const RESYNC_GAP: Duration = Duration::from_millis(500);
/// History in a snapshot, at most: all of it arrives before the client's
/// terminal shows any (synchronized output holds it, for up to a second).
const HISTORY_BYTES: usize = 1 << 20;
/// History the client asked for itself, at most: it waits for it.
const MORE_HISTORY_BYTES: usize = 4 << 20;
/// The largest window tmux makes, either way.
const MAX_SIZE: u16 = 10000;
/// Input bytes per `send-keys`; paste bytes per `set-buffer`.
const KEYS_CHUNK: usize = 512;
const PASTE_CHUNK: usize = 4096;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Names each stream's paste buffer, so two attaches never share one.
static PASTES: AtomicU64 = AtomicU64::new(0);

/// Runs `cmd` (a `tmux -C attach-session`) as the stream's other end.
pub(super) fn open(mut cmd: Command, cols: u16, rows: u16, history: u32) -> Result<Stream> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().context("starting tmux control client")?;
    let stdin = child.stdin.take().context("tmux control client stdin")?;
    let stdout = child.stdout.take().context("tmux control client stdout")?;
    let flow = Arc::new(Flow::default());
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (ask_tx, ask_rx) = mpsc::unbounded_channel();
    let (sent_tx, sent_rx) = mpsc::unbounded_channel();
    let writer = Writer {
        input: Input::new(format!(
            "otter-paste-{}",
            PASTES.fetch_add(1, Ordering::Relaxed)
        )),
    };
    tokio::spawn(writer.run(stdin, in_rx, ask_rx, sent_tx));
    let reader = Reader::new(flow.clone(), ask_tx, size(cols, rows), history);
    tokio::spawn(reader.run(child, stdout, control_rx, sent_rx));
    Ok(Stream::new(flow, in_tx, control_tx))
}

/// What the reader asks the writer to send.
enum Ask {
    Snapshot(Take),
    Resize((u16, u16)),
}

/// A snapshot to take.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Take {
    size: (u16, u16),
    history: u32,
    /// Bytes of history, at most.
    budget: usize,
    /// Draw the screen at once, before the history: the client's terminal is
    /// new or out of date. Not when it asked for more history (it's reading).
    screen_first: bool,
}

/// A line sent to tmux: how many replies it gets, and what it was.
struct Sent {
    replies: usize,
    line: Line,
}

enum Line {
    Input,
    /// Ends with the window's size.
    Resize,
    Snapshot(Take),
}

/// Writes tmux commands: the client's typing, and what the reader asks.
struct Writer {
    input: Input,
}

impl Writer {
    async fn run(
        mut self,
        mut stdin: ChildStdin,
        mut input: mpsc::UnboundedReceiver<Vec<u8>>,
        mut asks: mpsc::UnboundedReceiver<Ask>,
        sent: mpsc::UnboundedSender<Sent>,
    ) {
        loop {
            let lines: Vec<(String, Sent)> = tokio::select! {
                data = input.recv() => match data {
                    Some(data) => self
                        .input
                        .commands(&data)
                        .into_iter()
                        .map(|line| (line, Sent { replies: 1, line: Line::Input }))
                        .collect(),
                    // Detached: closing stdin ends the control client, and
                    // nothing reaches the process.
                    None => return,
                },
                Some(ask) = asks.recv() => vec![match ask {
                    Ask::Snapshot(take) => (
                        snapshot_line(take.size, take.history),
                        Sent { replies: SNAPSHOT_REPLIES, line: Line::Snapshot(take) },
                    ),
                    Ask::Resize((cols, rows)) => (
                        format!(
                            "refresh-client -C {cols}x{rows} ; \
                             display-message -p '#{{window_width}}x#{{window_height}}'"
                        ),
                        Sent { replies: RESIZE_REPLIES, line: Line::Resize },
                    ),
                }],
            };
            for (line, record) in lines {
                // An empty line would detach the client.
                debug_assert!(!line.is_empty());
                // Recorded before it's sent, so it's there when the reply is.
                if sent.send(record).is_err()
                    || stdin
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// The command line that takes a snapshot. It sets the client's size first:
/// the window is that size for the commands after it, unless another client
/// sizes it.
fn snapshot_line((cols, rows): (u16, u16), history: u32) -> String {
    format!(
        "refresh-client -C {cols}x{rows} ; display-message -p '{MARKER} {STATE_FORMAT}' ; \
         capture-pane -p -e -J -S -{history} -E -1 ; capture-pane -p -e -N ; \
         capture-pane -p -e -N -a -q",
        history = history.max(1),
    )
}

/// Reads what tmux sends and keeps what the client is.
struct Reader {
    relay: Relay,
    flow: Arc<Flow>,
    asks: mpsc::UnboundedSender<Ask>,
    /// The client's size, as it last said.
    size: (u16, u16),
    /// Lines of history the client keeps.
    history: u32,
    /// The window's size, as tmux last said.
    window: Option<(u16, u16)>,
    /// Lines of ours that resize the window, not yet answered: until then
    /// the window's size is in flux.
    resizing: usize,
    /// What the client was last told about the window.
    told: Option<(u16, u16)>,
    /// A snapshot was asked for and hasn't come.
    taking: bool,
    /// A snapshot to take once the one coming has come.
    next: Option<Take>,
    /// When the last snapshot was asked for.
    asked: Instant,
    /// Fell behind: a snapshot once the client has caught up, and not before
    /// `resync_at`.
    resync: bool,
    resync_at: Option<Instant>,
}

impl Reader {
    fn new(
        flow: Arc<Flow>,
        asks: mpsc::UnboundedSender<Ask>,
        size: (u16, u16),
        history: u32,
    ) -> Self {
        Reader {
            relay: Relay::new(),
            flow,
            asks,
            size,
            history: history.min(HISTORY_LIMIT),
            window: None,
            resizing: 0,
            told: None,
            taking: false,
            next: None,
            asked: Instant::now(),
            resync: false,
            resync_at: None,
        }
    }

    async fn run(
        mut self,
        mut child: Child,
        stdout: ChildStdout,
        mut controls: mpsc::UnboundedReceiver<StreamInput>,
        mut sent: mpsc::UnboundedReceiver<Sent>,
    ) {
        self.take(self.regular(true));
        let flow = self.flow.clone();
        let mut parser = Parser::default();
        let mut pending: VecDeque<(Sent, Vec<Reply>)> = VecDeque::new();
        let mut stdout = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            let resync_at = self.resync_at;
            tokio::select! {
                read = stdout.read_until(b'\n', &mut line) => {
                    if !matches!(read, Ok(n) if n > 0) {
                        break;
                    }
                }
                // `line` keeps what was read of a line if these win.
                more = flow.client(self.resync) => {
                    if !more {
                        break;
                    }
                    self.resync = false;
                    self.resync_at = Some(Instant::now().max(self.asked + RESYNC_GAP));
                    continue;
                }
                () = async { tokio::time::sleep_until(resync_at.unwrap_or_else(Instant::now)).await },
                    if resync_at.is_some() =>
                {
                    self.resync_at = None;
                    self.take(self.regular(true));
                    continue;
                }
                Some(control) = controls.recv() => {
                    self.control(control);
                    continue;
                }
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            let event = parser.line(&line);
            line.clear();
            match event {
                Some(Event::Output { pane, data }) => match self.relay.output(&pane, data) {
                    Some(data)
                        if flow.bytes() + data.len() > BEHIND && self.relay.fell_behind() =>
                    {
                        // Not taken in time: a snapshot replaces all of it.
                        flow.clear();
                        self.resync = true;
                    }
                    Some(data) => {
                        if !flow.push_output(data) {
                            break;
                        }
                    }
                    None => {}
                },
                Some(Event::Layout { size }) => {
                    self.window = Some(size);
                    self.tell();
                }
                Some(Event::Reply { ok, lines }) => {
                    while let Ok(record) = sent.try_recv() {
                        pending.push_back((record, Vec::new()));
                    }
                    let Some((record, replies)) = pending.front_mut() else {
                        continue;
                    };
                    replies.push(Reply { ok, lines });
                    if ok && replies.len() < record.replies {
                        continue;
                    }
                    if let Some((record, replies)) = pending.pop_front()
                        && !self.answered(record.line, replies)
                    {
                        break;
                    }
                }
                None => {}
            }
        }
        flow.done();
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    /// A line's replies came; `false` once the client has left.
    fn answered(&mut self, line: Line, replies: Vec<Reply>) -> bool {
        match line {
            Line::Input => true,
            Line::Resize => {
                self.resizing = self.resizing.saturating_sub(1);
                if let Some(size) = replies
                    .get(1)
                    .filter(|r| r.ok)
                    .and_then(|r| r.lines.first())
                    .and_then(|l| parse_size(l))
                {
                    self.window = Some(size);
                }
                self.tell();
                true
            }
            Line::Snapshot(take) => {
                self.resizing = self.resizing.saturating_sub(1);
                self.taking = false;
                let Some(shot) = self.relay.snapshot(replies, take) else {
                    if let Some(next) = self.next.take() {
                        self.take(next);
                    }
                    return true;
                };
                self.window = Some(shot.size);
                // Before the snapshot: the client lays it out at that size.
                self.tell();
                let pushed = self.flow.push(StreamOutput::Data(shot.bytes))
                    && self.flow.push(StreamOutput::Snapshot {
                        lines: shot.lines,
                        more: shot.more,
                    });
                if let Some(next) = self.next.take() {
                    self.take(next);
                }
                pushed
            }
        }
    }

    fn control(&mut self, control: StreamInput) {
        match control {
            StreamInput::Resize { cols, rows } => self.resize(size(cols, rows)),
            StreamInput::View {
                visible,
                cols,
                rows,
            } => {
                self.relay.visible = visible;
                if !visible {
                    return;
                }
                // Back on screen at this size; a snapshot if output was missed.
                let size = size(cols, rows);
                if std::mem::take(&mut self.relay.dropped) {
                    self.size = size;
                    self.take(self.regular(true));
                } else {
                    self.resize(size);
                }
            }
            StreamInput::History(lines) => {
                self.history = lines.min(HISTORY_LIMIT);
                self.take(Take {
                    budget: MORE_HISTORY_BYTES,
                    ..self.regular(false)
                });
            }
            StreamInput::Data(_) => {}
        }
    }

    fn regular(&self, screen_first: bool) -> Take {
        Take {
            size: self.size,
            history: self.history,
            budget: HISTORY_BYTES,
            screen_first,
        }
    }

    /// Take a snapshot, or once the one coming has come.
    fn take(&mut self, take: Take) {
        if self.taking {
            let next = self.next.get_or_insert(take);
            next.history = next.history.max(take.history);
            next.budget = next.budget.max(take.budget);
            next.screen_first |= take.screen_first;
            next.size = take.size;
            return;
        }
        self.taking = true;
        self.relay.wait();
        self.resizing += 1;
        self.asked = Instant::now();
        let _ = self.asks.send(Ask::Snapshot(Take {
            size: self.size,
            ..take
        }));
    }

    fn resize(&mut self, size: (u16, u16)) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.resizing += 1;
        let _ = self.asks.send(Ask::Resize(size));
    }

    /// Tell the client the window's size if another client sizes it, or that
    /// it no longer does, once our own resizes have settled.
    fn tell(&mut self) {
        if self.resizing > 0 {
            return;
        }
        let other = self.window.filter(|w| *w != self.size);
        if other != self.told {
            self.told = other;
            self.flow.push(StreamOutput::Window(other));
        }
    }
}

/// Some clients report 0×0 when they can't tell; don't shrink the session's
/// window to nothing because of it. tmux refuses more than [`MAX_SIZE`].
fn size(cols: u16, rows: u16) -> (u16, u16) {
    if cols == 0 || rows == 0 {
        (80, 24)
    } else {
        (cols.min(MAX_SIZE), rows.min(MAX_SIZE))
    }
}

/// `100x30` → (100, 30).
fn parse_size(text: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(text).ok()?;
    let (cols, rows) = text.trim().split_once('x')?;
    Some((cols.parse().ok()?, rows.parse().ok()?))
}

/// One line of control-mode output, read.
#[derive(Debug, PartialEq)]
enum Event {
    /// The reply to one of this client's commands, `ok` unless it failed.
    Reply { ok: bool, lines: Vec<Vec<u8>> },
    /// The pane's own output.
    Output { pane: Vec<u8>, data: Vec<u8> },
    /// The window's layout changed; it's this size now.
    Layout { size: (u16, u16) },
}

struct Reply {
    ok: bool,
    lines: Vec<Vec<u8>>,
}

/// Reads control-mode lines. A reply runs from `%begin` to the `%end` (or
/// `%error`) with the same arguments, so a line of pane text that happens
/// to start with `%end` can't end it. Replies to commands that weren't this
/// client's (flags 0: the attach itself) are skipped.
#[derive(Default)]
struct Parser {
    reply: Option<(Vec<u8>, Vec<Vec<u8>>)>,
}

impl Parser {
    fn line(&mut self, line: &[u8]) -> Option<Event> {
        if let Some((guard, lines)) = &mut self.reply {
            for (end, ok) in [(&b"%end "[..], true), (b"%error ", false)] {
                if line.strip_prefix(end) == Some(guard.as_slice()) {
                    let (guard, lines) = self.reply.take()?;
                    let ours = guard.rsplit(|&b| b == b' ').next() == Some(b"1");
                    return ours.then_some(Event::Reply { ok, lines });
                }
            }
            lines.push(line.to_vec());
            return None;
        }
        if let Some(guard) = line.strip_prefix(b"%begin ") {
            self.reply = Some((guard.to_vec(), Vec::new()));
            return None;
        }
        // `%layout-change @1 a87d,100x30,0,0,0 …`: after the checksum, the size.
        if let Some(rest) = line.strip_prefix(b"%layout-change ") {
            let layout = rest.split(|&b| b == b' ').nth(1)?;
            let size = layout.split(|&b| b == b',').nth(1)?;
            return Some(Event::Layout {
                size: parse_size(size)?,
            });
        }
        let rest = line.strip_prefix(b"%output ")?;
        let space = rest.iter().position(|&b| b == b' ')?;
        Some(Event::Output {
            pane: rest[..space].to_vec(),
            data: unescape(&rest[space + 1..]),
        })
    }
}

/// `%output` escapes control bytes and backslash as `\ooo`.
fn unescape(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        if escaped[i] == b'\\'
            && let Some(digits) = escaped.get(i + 1..i + 4)
            && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        {
            out.push(
                digits
                    .iter()
                    .fold(0u8, |n, d| n.wrapping_mul(8).wrapping_add(d - b'0')),
            );
            i += 4;
        } else {
            out.push(escaped[i]);
            i += 1;
        }
    }
    out
}

/// The pane's terminal state, from the snapshot's state reply.
#[derive(Clone, Debug, Default, PartialEq)]
struct State {
    pane: Vec<u8>,
    width: u16,
    height: u16,
    history: u32,
    alternate: bool,
    saved_x: u16,
    saved_y: u16,
    cursor_x: u16,
    cursor_y: u16,
    cursor_visible: bool,
    cursor_keys: bool,
    keypad: bool,
    wrap: bool,
    insert: bool,
    origin: bool,
    region_top: u16,
    region_bottom: u16,
    mouse_standard: bool,
    mouse_button: bool,
    mouse_all: bool,
    mouse_utf8: bool,
    mouse_sgr: bool,
    /// DECSCUSR's number for the cursor's shape, if the program set one
    /// (tmux 3.4+ says).
    cursor_style: Option<u8>,
    /// The cursor's colour, if the program set one.
    cursor_colour: Option<String>,
}

impl State {
    /// A terminal as it starts out, at the client's size.
    fn fallback((width, height): (u16, u16)) -> State {
        State {
            width,
            height,
            cursor_visible: true,
            wrap: true,
            region_bottom: height.saturating_sub(1),
            ..State::default()
        }
    }

    /// `line` is the marker followed by [`STATE_FORMAT`], expanded.
    fn parse(line: &[u8]) -> Option<State> {
        let line = std::str::from_utf8(line).ok()?;
        let f: Vec<&str> = line.strip_prefix(MARKER)?.trim_start().split(',').collect();
        if f.len() < 22 {
            return None;
        }
        // Unset positions come as 4294967295 (the saved cursor, when there's
        // no alternate screen).
        let n = |i: usize| {
            f[i].parse::<u64>()
                .ok()
                .map(|v| v.min(u64::from(u16::MAX)) as u16)
        };
        let on = |i: usize| f[i] != "0";
        let blinking = f.get(23).map(|b| *b == "1");
        let cursor_style = match (f.get(22).copied(), blinking) {
            (Some("block"), Some(blink)) => Some(if blink { 1 } else { 2 }),
            (Some("underline"), Some(blink)) => Some(if blink { 3 } else { 4 }),
            (Some("bar"), Some(blink)) => Some(if blink { 5 } else { 6 }),
            _ => None,
        };
        let cursor_colour = f
            .get(24)
            .filter(|c| !c.is_empty() && **c != "none")
            .map(|c| (*c).to_owned());
        Some(State {
            pane: f[0].as_bytes().to_vec(),
            width: n(1)?,
            height: n(2)?,
            history: f[3].parse().ok()?,
            alternate: on(4),
            saved_x: n(5)?,
            saved_y: n(6)?,
            cursor_x: n(7)?,
            cursor_y: n(8)?,
            cursor_visible: on(9),
            cursor_keys: on(10),
            keypad: on(11),
            wrap: on(12),
            insert: on(13),
            origin: on(14),
            region_top: n(15)?,
            region_bottom: n(16)?,
            mouse_standard: on(17),
            mouse_button: on(18),
            mouse_all: on(19),
            mouse_utf8: on(20),
            mouse_sgr: on(21),
            cursor_style,
            cursor_colour,
        })
    }
}

/// A snapshot's replies: state, history, the screen shown, and the normal
/// screen behind the alternate one (empty when there's none).
struct Snapshot {
    state: State,
    history: Vec<Vec<u8>>,
    screen: Vec<Vec<u8>>,
    saved: Vec<Vec<u8>>,
}

/// A snapshot rendered for the client.
struct Shot {
    bytes: Vec<u8>,
    /// The window's size it was taken at.
    size: (u16, u16),
    lines: u32,
    more: bool,
}

/// What the client should see of the pane's output.
struct Relay {
    /// A snapshot is coming; output until it is in it.
    coming: bool,
    pane: Vec<u8>,
    /// The client's terminal is on screen; not, its output is dropped.
    visible: bool,
    /// Output was dropped while it wasn't.
    dropped: bool,
}

impl Relay {
    fn new() -> Self {
        Relay {
            coming: true,
            pane: Vec::new(),
            visible: true,
            dropped: false,
        }
    }

    /// A snapshot was asked for: skip output until it.
    fn wait(&mut self) {
        self.coming = true;
    }

    /// A session has one pane; its id is known unless its state wasn't.
    fn output(&mut self, pane: &[u8], data: Vec<u8>) -> Option<Vec<u8>> {
        if self.coming || !(pane == self.pane || self.pane.is_empty()) {
            return None;
        }
        if !self.visible {
            self.dropped = true;
            return None;
        }
        Some(data)
    }

    /// The replies to a snapshot's line: what rebuilds the client's terminal.
    /// A failed command loses its reply and the ones after it; what came is
    /// still shown.
    fn snapshot(&mut self, replies: Vec<Reply>, take: Take) -> Option<Shot> {
        if !self.coming {
            return None;
        }
        let mut parts: Vec<Option<Vec<Vec<u8>>>> = replies
            .into_iter()
            .map(|r| r.ok.then_some(r.lines))
            .collect();
        if parts.len() < SNAPSHOT_REPLIES || parts.iter().any(Option::is_none) {
            tracing::warn!("stream attach: part of the snapshot failed");
        }
        let mut part = |i: usize| parts.get_mut(i).and_then(Option::take).unwrap_or_default();
        let state_reply = part(1);
        let state = state_reply
            .first()
            .and_then(|l| State::parse(l))
            .unwrap_or_else(|| {
                tracing::warn!(
                    reply = %String::from_utf8_lossy(&state_reply.concat()),
                    "stream attach: unexpected pane state"
                );
                State::fallback(take.size)
            });
        let snapshot = Snapshot {
            state,
            history: part(2),
            screen: part(3),
            saved: part(4),
        };
        self.pane = snapshot.state.pane.clone();
        self.coming = false;
        let (bytes, lines, more) = render(&snapshot, take);
        Some(Shot {
            bytes,
            size: (snapshot.state.width, snapshot.state.height),
            lines,
            more,
        })
    }

    /// The client fell behind: skip output until a fresh snapshot, if one
    /// isn't already coming.
    fn fell_behind(&mut self) -> bool {
        let streaming = !self.coming;
        self.coming = true;
        streaming
    }
}

/// Bytes that make the client's terminal show `snap` as `take` asked, the
/// lines of history in them, and whether the session holds earlier ones.
///
/// tmux's captures write a style once and it holds, across rows, until it
/// changes; each capture starts from the default style. A line feed that
/// scrolls fills the new line with the current background, so rows are
/// separated with the style reset and set again after.
fn render(snap: &Snapshot, take: Take) -> (Vec<u8>, u32, bool) {
    let s = &snap.state;
    let height = usize::from(s.height);
    let shown = rows(&snap.screen, height);
    let mut out = Vec::new();
    let (mut lines_sent, mut more) = (0, false);
    if take.screen_first {
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        out.extend_from_slice(LINE_DRAWING);
        draw(&mut out, &shown);
        cup(&mut out, s.cursor_y, s.cursor_x);
    }
    // CAN ends whatever sequence was cut off, RIS starts over, and what
    // follows shows only once it's all there.
    out.extend_from_slice(b"\x18\x1bc\x1b[?2026h");
    out.extend_from_slice(LINE_DRAWING);
    if take.history > 0 && s.history > 0 {
        let (dropped, kept) = budget(&snap.history, take.budget);
        lines_sent = kept.len() as u32;
        more = !dropped.is_empty() || s.history > take.history;
        // The first row kept goes on in the style the rows left out ended in.
        let mut style = Style::default();
        for row in dropped {
            style.feed(row);
        }
        for row in kept {
            lines(&mut out, std::slice::from_ref(row), &mut style);
            out.extend_from_slice(b"\r\n");
        }
        end_link(&mut out, kept);
    }
    let normal = if s.alternate {
        rows(&snap.saved, height)
    } else {
        shown.clone()
    };
    lines(&mut out, &normal, &mut Style::default());
    end_link(&mut out, &normal);
    if s.alternate {
        cup(&mut out, s.saved_y, s.saved_x);
        out.extend_from_slice(b"\x1b[?1049h");
        draw(&mut out, &shown);
    }
    let full = s.region_top == 0 && usize::from(s.region_bottom) + 1 >= height;
    if !full && s.region_bottom > s.region_top {
        let _ = write!(
            Bytes(&mut out),
            "\x1b[{};{}r",
            s.region_top + 1,
            s.region_bottom + 1
        );
    }
    for (on, mode) in [
        (s.origin, &b"\x1b[?6h"[..]),
        (!s.wrap, b"\x1b[?7l"),
        (s.insert, b"\x1b[4h"),
        (s.cursor_keys, b"\x1b[?1h"),
        (s.keypad, b"\x1b="),
        (s.mouse_standard, b"\x1b[?1000h"),
        (s.mouse_button, b"\x1b[?1002h"),
        (s.mouse_all, b"\x1b[?1003h"),
        (s.mouse_utf8, b"\x1b[?1005h"),
        (s.mouse_sgr, b"\x1b[?1006h"),
        (true, b"\x1b[?2004h"),
        (!s.cursor_visible, b"\x1b[?25l"),
    ] {
        if on {
            out.extend_from_slice(mode);
        }
    }
    if let Some(style) = s.cursor_style {
        let _ = write!(Bytes(&mut out), "\x1b[{style} q");
    }
    if let Some(colour) = &s.cursor_colour {
        let _ = write!(Bytes(&mut out), "\x1b]12;{colour}\x07");
    }
    let top = if s.origin { s.region_top } else { 0 };
    cup(&mut out, s.cursor_y.saturating_sub(top), s.cursor_x);
    out.extend_from_slice(b"\x1b[?2026l");
    (out, lines_sent, more)
}

/// tmux marks cells in the DEC line-drawing set with SO and SI (shift out to
/// G1 and back), so G1 is that set, as tmux sets it for a terminal client.
const LINE_DRAWING: &[u8] = b"\x1b)0";

/// A capture's rows may end inside a hyperlink (OSC 8), which the next
/// capture doesn't expect.
fn end_link(out: &mut Vec<u8>, rows: &[Vec<u8>]) {
    if rows.iter().any(|row| find(row, b"\x1b]8;").is_some()) {
        out.extend_from_slice(b"\x1b]8;;\x1b\\");
    }
}

/// `lines` as exactly `height` rows of the screen, without the marker tmux
/// draws under an exited process (its styles stay: later rows go on in them).
fn rows(lines: &[Vec<u8>], height: usize) -> Vec<Vec<u8>> {
    let mut rows: Vec<Vec<u8>> = lines
        .iter()
        .take(height)
        .map(|line| {
            let plain = plain(line);
            if plain.starts_with(EXIT_MARKER.as_bytes())
                || plain.starts_with(OLD_EXIT_MARKER.as_bytes())
            {
                csi(line)
            } else {
                line.clone()
            }
        })
        .collect();
    rows.resize(height, Vec::new());
    rows
}

/// Rows one under the other, each in the style the previous ended in.
fn lines(out: &mut Vec<u8>, rows: &[Vec<u8>], style: &mut Style) {
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        row_in(out, row, style);
    }
}

/// Rows each at its place, so nothing scrolls.
fn draw(out: &mut Vec<u8>, rows: &[Vec<u8>]) {
    let mut style = Style::default();
    for (i, row) in rows.iter().enumerate() {
        cup(out, i as u16, 0);
        row_in(out, row, &mut style);
    }
    end_link(out, rows);
}

/// `row` in `style`, which it leaves as it ends; the terminal is left in the
/// default style.
fn row_in(out: &mut Vec<u8>, row: &[u8], style: &mut Style) {
    out.extend_from_slice(&style.sgr());
    out.extend_from_slice(row);
    out.extend_from_slice(b"\x1b[0m");
    style.feed(row);
    if style.shifted {
        out.push(SI);
    }
}

fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    let _ = write!(Bytes(out), "\x1b[{};{}H", row + 1, col + 1);
}

/// The latest history lines that fit in `max` bytes, and those before them.
fn budget(lines: &[Vec<u8>], max: usize) -> (&[Vec<u8>], &[Vec<u8>]) {
    let mut total = 0;
    let start = lines
        .iter()
        .rposition(|l| {
            total += l.len() + 16;
            total > max
        })
        .map_or(0, |i| i + 1);
    lines.split_at(start)
}

/// CSI sequences of `line`, as (parameters, final byte), and its text.
fn split_csi(line: &[u8]) -> (Vec<(&[u8], u8)>, Vec<u8>) {
    let mut csi = Vec::new();
    let mut text = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        if line[i] == 0x1b && line.get(i + 1) == Some(&b'[') {
            let start = i + 2;
            let mut end = start;
            while end < line.len() && !(0x40..=0x7e).contains(&line[end]) {
                end += 1;
            }
            if let Some(&last) = line.get(end) {
                csi.push((&line[start..end], last));
            }
            i = end + 1;
        } else {
            text.push(line[i]);
            i += 1;
        }
    }
    (csi, text)
}

fn plain(line: &[u8]) -> Vec<u8> {
    split_csi(line).1
}

/// Only the CSI sequences of `line`.
fn csi(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (params, last) in split_csi(line).0 {
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(params);
        out.push(last);
    }
    out
}

/// The style (SGR) in effect after some output, kept short: what each
/// attribute ends up as, not every change on the way.
#[derive(Clone, Debug, Default, PartialEq)]
struct Style {
    fg: Option<String>,
    bg: Option<String>,
    underline_color: Option<String>,
    /// Attributes on (`1`, `3`, `4:3`, `7`…), in the order turned on.
    attrs: Vec<String>,
    /// Shifted out: line drawing (SO), until SI. SGR doesn't touch it.
    shifted: bool,
}

const SO: u8 = 0x0e;
const SI: u8 = 0x0f;

impl Style {
    fn feed(&mut self, line: &[u8]) {
        let mut i = 0;
        while i < line.len() {
            match line[i] {
                0x1b if line.get(i + 1) == Some(&b'[') => {
                    let start = i + 2;
                    let mut end = start;
                    while end < line.len() && !(0x40..=0x7e).contains(&line[end]) {
                        end += 1;
                    }
                    if line.get(end) == Some(&b'm') {
                        self.apply(&String::from_utf8_lossy(&line[start..end]));
                    }
                    i = end + 1;
                    continue;
                }
                SO => self.shifted = true,
                SI => self.shifted = false,
                _ => {}
            }
            i += 1;
        }
    }

    fn apply(&mut self, params: &str) {
        let mut it = params.split(';');
        while let Some(p) = it.next() {
            let code = p.split(':').next().unwrap_or("");
            match code {
                "" | "0" => {
                    *self = Style {
                        shifted: self.shifted,
                        ..Style::default()
                    }
                }
                "38" | "48" | "58" => {
                    // `38;5;n`, `38;2;r;g;b`, or the same with colons.
                    let color = if p.contains(':') {
                        p.to_owned()
                    } else {
                        let mut color = p.to_owned();
                        let kind = it.next().unwrap_or("");
                        color.push(';');
                        color.push_str(kind);
                        for _ in 0..if kind == "2" { 3 } else { 1 } {
                            color.push(';');
                            color.push_str(it.next().unwrap_or("0"));
                        }
                        color
                    };
                    match code {
                        "38" => self.fg = Some(color),
                        "48" => self.bg = Some(color),
                        _ => self.underline_color = Some(color),
                    }
                }
                "39" => self.fg = None,
                "49" => self.bg = None,
                "59" => self.underline_color = None,
                c if matches!(c.len(), 2 | 3)
                    && matches!(c.parse::<u16>(), Ok(30..=37 | 90..=97)) =>
                {
                    self.fg = Some(c.to_owned());
                }
                c if matches!(c.parse::<u16>(), Ok(40..=47 | 100..=107)) => {
                    self.bg = Some(c.to_owned());
                }
                "22" => self.off(&["1", "2"]),
                "23" => self.off(&["3"]),
                "24" => self.off(&["4", "21"]),
                "25" => self.off(&["5", "6"]),
                "27" => self.off(&["7"]),
                "28" => self.off(&["8"]),
                "29" => self.off(&["9"]),
                "55" => self.off(&["53"]),
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "21" | "53" => {
                    // One kind of underline at a time.
                    self.off(&[code]);
                    if code == "21" {
                        self.off(&["4"]);
                    }
                    self.attrs.push(p.to_owned());
                }
                _ => {}
            }
        }
    }

    fn off(&mut self, codes: &[&str]) {
        self.attrs
            .retain(|a| !codes.contains(&a.split(':').next().unwrap_or("")));
    }

    /// What sets this style from the default; empty for the default.
    fn sgr(&self) -> Vec<u8> {
        let mut params: Vec<&str> = self.attrs.iter().map(String::as_str).collect();
        params.extend(self.fg.as_deref());
        params.extend(self.bg.as_deref());
        params.extend(self.underline_color.as_deref());
        let mut out = if params.is_empty() {
            Vec::new()
        } else {
            format!("\x1b[{}m", params.join(";")).into_bytes()
        };
        if self.shifted {
            out.push(SO);
        }
        out
    }
}

/// `write!` into a byte buffer.
struct Bytes<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Bytes<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// The client's input → tmux commands, minding bracketed pastes.
struct Input {
    buffer: String,
    /// Inside a bracketed paste: what it holds so far.
    paste: Option<Vec<u8>>,
}

impl Input {
    fn new(buffer: String) -> Self {
        Input {
            buffer,
            paste: None,
        }
    }

    fn commands(&mut self, mut data: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            if let Some(paste) = &mut self.paste {
                let Some(end) = find(data, PASTE_END) else {
                    paste.extend_from_slice(data);
                    break;
                };
                paste.extend_from_slice(&data[..end]);
                let paste = self.paste.take().unwrap_or_default();
                self.paste_commands(&paste, &mut out);
                data = &data[end + PASTE_END.len()..];
            } else {
                let Some(start) = find(data, PASTE_START) else {
                    keys(data, &mut out);
                    break;
                };
                keys(&data[..start], &mut out);
                self.paste = Some(Vec::new());
                data = &data[start + PASTE_START.len()..];
            }
        }
        out
    }

    /// tmux adds the brackets if the program asked for them (`-p`), and
    /// keeps line feeds as they are (`-r`).
    fn paste_commands(&self, paste: &[u8], out: &mut Vec<String>) {
        if paste.is_empty() {
            return;
        }
        // tmux's strings end at a NUL; such a paste goes as typing.
        if paste.contains(&0) {
            keys(paste, out);
            return;
        }
        for (i, chunk) in utf8_chunks(paste, PASTE_CHUNK).enumerate() {
            let append = if i == 0 { "" } else { " -a" };
            out.push(format!(
                "set-buffer{append} -b {} \"{}\"",
                self.buffer,
                quote(chunk)
            ));
        }
        out.push(format!("paste-buffer -p -r -d -b {}", self.buffer));
    }
}

/// Typed bytes, as they are. Home and End go as keys tmux translates for
/// the program's terminal (`\e[1~`, `\e[4~`), as it does for a terminal
/// client; the client's terminal sends xterm's.
fn keys(data: &[u8], out: &mut Vec<String>) {
    match data {
        b"\x1b[H" | b"\x1bOH" => return out.push("send-keys Home".into()),
        b"\x1b[F" | b"\x1bOF" => return out.push("send-keys End".into()),
        _ => {}
    }
    for chunk in data.chunks(KEYS_CHUNK) {
        let mut line = String::from("send-keys -H");
        for b in chunk {
            let _ = write!(line, " {b:02x}");
        }
        out.push(line);
    }
}

/// `bytes` inside a tmux double-quoted string.
fn quote(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' | '"' | '$' => {
                    s.push('\\');
                    s.push(c);
                }
                c if c < ' ' || c == '\x7f' => {
                    let _ = write!(s, "\\{:03o}", c as u32);
                }
                c => s.push(c),
            }
        }
        for b in chunk.invalid() {
            let _ = write!(s, "\\{b:03o}");
        }
    }
    s
}

/// `bytes` in pieces of at most `max`, not splitting a UTF-8 character.
fn utf8_chunks(bytes: &[u8], max: usize) -> impl Iterator<Item = &[u8]> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(max);
        while end < rest.len() && end > 1 && rest[end] & 0xc0 == 0x80 {
            end -= 1;
        }
        let (chunk, tail) = rest.split_at(end);
        rest = tail;
        Some(chunk)
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(text: &[&str]) -> Vec<Vec<u8>> {
        text.iter().map(|l| l.as_bytes().to_vec()).collect()
    }

    #[test]
    fn replies_end_only_at_their_own_end() {
        let mut p = Parser::default();
        assert_eq!(p.line(b"%begin 1 7 1"), None);
        assert_eq!(p.line(b"%end 1 6 1"), None);
        assert_eq!(p.line(b"text"), None);
        assert_eq!(
            p.line(b"%end 1 7 1"),
            Some(Event::Reply {
                ok: true,
                lines: lines_of(&["%end 1 6 1", "text"])
            })
        );
        assert_eq!(p.line(b"%begin 2 8 1"), None);
        assert_eq!(
            p.line(b"%error 2 8 1"),
            Some(Event::Reply {
                ok: false,
                lines: vec![]
            })
        );
        // The attach's own reply isn't one of ours.
        assert_eq!(p.line(b"%begin 2 9 0"), None);
        assert_eq!(p.line(b"%end 2 9 0"), None);
        assert_eq!(p.line(b"%session-changed $0 x"), None);
        assert_eq!(
            p.line(b"%output %3 X\\134Y\\033[31mZ\\011\\015\\012\\001"),
            Some(Event::Output {
                pane: b"%3".to_vec(),
                data: b"X\\Y\x1b[31mZ\t\r\n\x01".to_vec()
            })
        );
    }

    fn state_line(alternate: bool) -> String {
        format!(
            "{MARKER} %3,20,3,2,{},4,1,5,2,1,1,0,1,0,0,0,2,0,0,1,0,1,default,0,none",
            u8::from(alternate)
        )
    }

    fn take(size: (u16, u16), history: u32, screen_first: bool) -> Take {
        Take {
            size,
            history,
            budget: HISTORY_BYTES,
            screen_first,
        }
    }

    fn reply(lines: &[&str]) -> Reply {
        Reply {
            ok: true,
            lines: lines_of(lines),
        }
    }

    fn replies(state: &str, history: &[&str], screen: &[&str]) -> Vec<Reply> {
        vec![
            reply(&[]),
            reply(&[state]),
            reply(history),
            reply(screen),
            reply(&[]),
        ]
    }

    #[test]
    fn state_reads_unset_positions() {
        // tmux before 3.4 has no cursor formats: they come empty.
        let line =
            format!("{MARKER} %0,40,5,0,0,4294967295,4294967295,2,0,1,0,0,1,0,0,0,4,0,0,0,0,0,,,");
        let state = State::parse(line.as_bytes()).expect("parsed");
        assert!(!state.alternate);
        assert_eq!((state.cursor_x, state.region_bottom), (2, 4));
        assert_eq!(
            (state.cursor_style, state.cursor_colour.as_deref()),
            (None, None)
        );
        assert!(State::parse(b"otter-snapshot %0,40").is_none());
        let line = format!("{MARKER} %0,40,5,0,0,0,0,2,0,1,0,0,1,0,0,0,4,0,0,0,0,0,bar,0,#ff8800");
        let state = State::parse(line.as_bytes()).expect("parsed");
        assert_eq!(
            (state.cursor_style, state.cursor_colour.as_deref()),
            (Some(6), Some("#ff8800"))
        );
    }

    #[test]
    fn output_counts_only_after_the_snapshot() {
        let mut relay = Relay::new();
        assert!(
            snapshot_line((20, 3), 10)
                .starts_with("refresh-client -C 20x3 ; display-message -p 'otter-snapshot ")
        );
        assert_eq!(
            snapshot_line((20, 3), 10).matches(" ; ").count() + 1,
            SNAPSHOT_REPLIES
        );
        assert_eq!(relay.output(b"%3", b"before".to_vec()), None);
        let shot = relay
            .snapshot(
                replies(&state_line(false), &["old line"], &["$ ls", "a  b"]),
                take((20, 3), 10, true),
            )
            .unwrap();
        assert_eq!((shot.size, shot.lines, shot.more), ((20, 3), 1, false));
        let shown = String::from_utf8(shot.bytes).unwrap();
        assert!(
            shown.contains("old line") && shown.contains("$ ls"),
            "{shown:?}"
        );
        assert_eq!(relay.output(b"%9", b"another pane".to_vec()), None);
        assert_eq!(
            relay.output(b"%3", b"after".to_vec()),
            Some(b"after".to_vec())
        );
    }

    #[test]
    fn a_failed_or_unreadable_snapshot_still_shows_what_came() {
        // An error drops the rest of the line: only the first reply came.
        let mut relay = Relay::new();
        let shown = relay
            .snapshot(
                vec![Reply {
                    ok: false,
                    lines: vec![],
                }],
                take((20, 2), 10, true),
            )
            .unwrap()
            .bytes;
        assert!(shown.ends_with(b"\x1b[?2026l"));
        assert_eq!(
            relay.output(b"%0", b"live".to_vec()),
            Some(b"live".to_vec())
        );

        let mut relay = Relay::new();
        let shown = relay
            .snapshot(
                replies("otter-snapshot %0,something new", &["old"], &["shown"]),
                take((20, 2), 10, true),
            )
            .unwrap()
            .bytes;
        let shown = String::from_utf8(shown).unwrap();
        // No history is claimed, so the first screen row isn't drawn twice.
        assert!(
            !shown.contains("old") && shown.contains("shown"),
            "{shown:?}"
        );
        assert_eq!(
            relay.output(b"%0", b"live".to_vec()),
            Some(b"live".to_vec())
        );
    }

    #[test]
    fn falling_behind_skips_output_until_a_new_snapshot() {
        let mut relay = Relay::new();
        let first = relay
            .snapshot(
                replies(&state_line(false), &[], &["screen"]),
                take((20, 3), 10, true),
            )
            .unwrap()
            .bytes;
        assert!(
            first.starts_with(b"\x1b[H\x1b[2J"),
            "the first draws at once"
        );
        assert!(relay.fell_behind());
        assert!(!relay.fell_behind(), "one snapshot at a time");
        assert_eq!(relay.output(b"%3", b"dropped".to_vec()), None);
        let again = relay
            .snapshot(
                replies(&state_line(false), &[], &["screen"]),
                take((20, 3), 10, false),
            )
            .unwrap()
            .bytes;
        assert!(again.starts_with(b"\x18\x1bc"));
        assert_eq!(
            relay.output(b"%3", b"live".to_vec()),
            Some(b"live".to_vec())
        );
    }

    fn snapshot(alternate: bool) -> Snapshot {
        Snapshot {
            state: State::parse(state_line(alternate).as_bytes()).unwrap(),
            history: lines_of(&["h1", "\x1b[31mh2  "]),
            screen: lines_of(&["one", "two   ", "[process exited, status 0]"]),
            saved: lines_of(&["n1", "n2", "n3"]),
        }
    }

    #[test]
    fn renders_history_screen_cursor_and_modes() {
        let state = &snapshot(false).state;
        assert_eq!((state.width, state.height, state.history), (20, 3, 2));
        assert_eq!((state.cursor_x, state.cursor_y), (5, 2));
        assert!(state.cursor_visible && state.cursor_keys && !state.keypad);
        assert_eq!((state.region_top, state.region_bottom), (0, 2));
        assert!(state.mouse_all && state.mouse_sgr && !state.mouse_standard);

        let out = String::from_utf8(render(&snapshot(false), take((20, 3), 10, false)).0).unwrap();
        assert_eq!(
            out,
            "\x18\x1bc\x1b[?2026h\x1b)0\
             h1\x1b[0m\r\n\x1b[31mh2  \x1b[0m\r\n\
             one\x1b[0m\r\ntwo   \x1b[0m\r\n\x1b[0m\
             \x1b[?1h\x1b[?1003h\x1b[?1006h\x1b[?2004h\
             \x1b[3;6H\x1b[?2026l"
        );
        let without =
            String::from_utf8(render(&snapshot(false), take((20, 3), 0, false)).0).unwrap();
        assert!(!without.contains("h1"), "history wasn't asked for");
    }

    #[test]
    fn renders_the_alternate_screen_over_the_normal_one() {
        let out = String::from_utf8(render(&snapshot(true), take((20, 3), 10, true)).0).unwrap();
        let (first, rest) = out.split_once("\x18\x1bc\x1b[?2026h\x1b)0").unwrap();
        assert_eq!(
            first,
            "\x1b[H\x1b[2J\x1b)0\x1b[1;1Hone\x1b[0m\x1b[2;1Htwo   \x1b[0m\x1b[3;1H\x1b[0m\x1b[3;6H"
        );
        assert!(rest.starts_with(
            "h1\x1b[0m\r\n\x1b[31mh2  \x1b[0m\r\nn1\x1b[0m\r\nn2\x1b[0m\r\nn3\x1b[0m"
        ));
        assert!(rest.contains(
            "\x1b[2;5H\x1b[?1049h\x1b[1;1Hone\x1b[0m\x1b[2;1Htwo   \x1b[0m\x1b[3;1H\x1b[0m"
        ));
    }

    #[test]
    fn styles_go_on_across_rows_as_tmux_captures_them() {
        // tmux writes a style once: rows 2 and 3 are blue too, until `49m`.
        let mut snap = snapshot(false);
        snap.screen = lines_of(&["\x1b[44m    ", "    ", "ab\x1b[49m  "]);
        snap.history = lines_of(&["\x1b[1;38;5;31mbold", "still", "\x1b[22mplain"]);
        let out = String::from_utf8(render(&snap, take((20, 3), 10, true)).0).unwrap();
        // Each row starts in the style the one before ended in.
        assert!(out.contains(
            "\x1b[1;1H\x1b[44m    \x1b[0m\x1b[2;1H\x1b[44m    \x1b[0m\x1b[3;1H\x1b[44mab\x1b[49m  \x1b[0m"
        ), "{out:?}");
        assert!(out.contains(
            "\x1b[1;38;5;31mbold\x1b[0m\r\n\x1b[1;38;5;31mstill\x1b[0m\r\n\x1b[1;38;5;31m\x1b[22mplain\x1b[0m\r\n"
        ), "{out:?}");
    }

    #[test]
    fn line_drawing_goes_on_across_rows_and_links_end_with_their_capture() {
        // tmux shifts out once; the next row's line drawing has no SO.
        let mut snap = snapshot(false);
        snap.screen = lines_of(&["\x0elqq", "qqk\x0f end", "\x1b]8;;https://x\x1b\\link"]);
        let out = String::from_utf8(render(&snap, take((20, 3), 0, false)).0).unwrap();
        assert!(out.contains("\x1b)0"), "G1 is line drawing");
        assert!(
            out.contains("\x0elqq\x1b[0m\x0f\r\n\x0eqqk\x0f end\x1b[0m\r\n"),
            "{out:?}"
        );
        assert!(out.contains("link\x1b[0m\x1b]8;;\x1b\\"), "{out:?}");
        let mut style = Style::default();
        style.feed(b"\x1b[1m\x0eab\x1b[0m");
        assert_eq!(style.sgr(), b"\x0e", "an SGR reset keeps the shift");
    }

    #[test]
    fn a_style_from_history_left_out_carries_on() {
        let history = lines_of(&["\x1b[7mlong", "aaaa", "bbbb"]);
        let (dropped, kept) = budget(&history, 45);
        assert_eq!((dropped.len(), kept.len()), (1, 2));
        let mut style = Style::default();
        for row in dropped {
            style.feed(row);
        }
        assert_eq!(style.sgr(), b"\x1b[7m");
    }

    #[test]
    fn style_keeps_what_each_attribute_ends_up_as() {
        let mut style = Style::default();
        style.feed(b"\x1b[1m\x1b[38;5;31ma\x1b[38;2;1;2;3m\x1b[48:5:9m\x1b[4:3m\x1b[21m");
        assert_eq!(style.sgr(), b"\x1b[1;21;38;2;1;2;3;48:5:9m");
        style.feed(b"\x1b[22;24;39;100m");
        assert_eq!(style.sgr(), b"\x1b[100m");
        style.feed(b"\x1b[m");
        assert!(style.sgr().is_empty());
    }

    #[test]
    fn history_is_kept_to_a_budget_from_the_end() {
        let history = lines_of(&["aaaa", "bbbb", "cccc"]);
        assert_eq!(budget(&history, 100).1.len(), 3);
        assert_eq!(budget(&history, 45).1, &history[1..]);
        assert!(budget(&history, 5).1.is_empty());
    }

    #[test]
    fn keys_go_as_bytes_and_pastes_through_a_buffer() {
        let mut input = Input::new("otter-paste-1".into());
        assert_eq!(
            input.commands(b"a\x1b[A\x03"),
            ["send-keys -H 61 1b 5b 41 03"]
        );
        assert!(input.commands(b"").is_empty(), "never an empty line");
        assert_eq!(input.commands(b"\x1b[H"), ["send-keys Home"]);
        assert_eq!(input.commands(b"\x1bOF"), ["send-keys End"]);
        assert_eq!(
            input.commands("x\x1b[200~l1\rsay \"hi\" $HOME \\ é\x1b[201~y".as_bytes()),
            [
                "send-keys -H 78",
                "set-buffer -b otter-paste-1 \"l1\\015say \\\"hi\\\" \\$HOME \\\\ é\"",
                "paste-buffer -p -r -d -b otter-paste-1",
                "send-keys -H 79",
            ]
        );
        // A paste split across frames.
        assert!(input.commands(b"\x1b[200~part one ").is_empty());
        assert_eq!(
            input.commands(b"part two\x1b[201~"),
            [
                "set-buffer -b otter-paste-1 \"part one part two\"",
                "paste-buffer -p -r -d -b otter-paste-1",
            ]
        );
        assert!(input.commands(b"\x1b[200~\x1b[201~").is_empty());
        assert_eq!(
            input.commands(b"\x1b[200~a\x00b\x1b[201~"),
            ["send-keys -H 61 00 62"],
            "a NUL can't be in a buffer's text"
        );
    }

    #[test]
    fn long_input_is_split_without_breaking_characters() {
        let mut input = Input::new("b".into());
        assert_eq!(input.commands(&[b'k'; KEYS_CHUNK + 1]).len(), 2);
        let text = "é".repeat(PASTE_CHUNK);
        let chunks: Vec<&[u8]> = utf8_chunks(text.as_bytes(), PASTE_CHUNK).collect();
        assert!(chunks.iter().all(|c| std::str::from_utf8(c).is_ok()));
        assert_eq!(chunks.concat(), text.as_bytes());
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend_from_slice(text.as_bytes());
        paste.extend_from_slice(b"\x1b[201~");
        let commands = input.commands(&paste);
        assert!(commands[0].starts_with("set-buffer -b b \""));
        assert!(commands[1].starts_with("set-buffer -a -b b \""));
        assert_eq!(commands.last().unwrap(), "paste-buffer -p -r -d -b b");
    }

    #[test]
    fn sizes_stay_within_what_tmux_takes() {
        assert_eq!(size(0, 50), (80, 24));
        assert_eq!(size(20000, 65535), (MAX_SIZE, MAX_SIZE));
        assert_eq!(size(100, 30), (100, 30));
    }

    /// A reader with no tmux behind it: what it asks for, and what it gives
    /// the client.
    fn reader() -> (Reader, mpsc::UnboundedReceiver<Ask>, Stream) {
        let flow = Arc::new(Flow::default());
        let (asks, asked) = mpsc::unbounded_channel();
        let (input, _) = mpsc::unbounded_channel();
        let (controls, _) = mpsc::unbounded_channel();
        let reader = Reader::new(flow.clone(), asks, (100, 30), 5000);
        (reader, asked, Stream::new(flow, input, controls))
    }

    fn snapshot_asked(asked: &mut mpsc::UnboundedReceiver<Ask>) -> Take {
        match asked.try_recv() {
            Ok(Ask::Snapshot(take)) => take,
            Ok(Ask::Resize(size)) => panic!("a resize to {size:?}, not a snapshot"),
            Err(_) => panic!("nothing asked"),
        }
    }

    /// The replies tmux gives a snapshot taken at `size` (cols, rows).
    fn shot_replies(size: (u16, u16)) -> Vec<Reply> {
        let state = format!(
            "{MARKER} %0,{},{},1,0,0,0,0,0,1,0,0,1,0,0,0,{},0,0,0,0,0,default,0,none",
            size.0,
            size.1,
            size.1 - 1
        );
        replies(&state, &["old"], &["screen"])
    }

    async fn next(stream: &mut Stream) -> StreamOutput {
        tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("something for the client")
            .expect("the stream goes on")
    }

    #[tokio::test]
    async fn one_snapshot_at_a_time_and_none_lost() {
        let (mut reader, mut asked, mut stream) = reader();
        reader.take(reader.regular(true));
        let first = snapshot_asked(&mut asked);
        assert_eq!(
            (first.size, first.history, first.screen_first),
            ((100, 30), 5000, true)
        );
        // Asked for more history while the first is coming: it waits for it.
        reader.control(StreamInput::History(20000));
        assert!(asked.try_recv().is_err());
        reader.answered(Line::Snapshot(first), shot_replies((100, 30)));
        assert!(matches!(next(&mut stream).await, StreamOutput::Data(_)));
        assert_eq!(
            next(&mut stream).await,
            StreamOutput::Snapshot {
                lines: 1,
                more: false
            }
        );
        let more = snapshot_asked(&mut asked);
        assert_eq!(
            (more.history, more.budget, more.screen_first),
            (20000, MORE_HISTORY_BYTES, false)
        );
    }

    #[tokio::test]
    async fn off_screen_output_is_dropped_and_a_snapshot_catches_up() {
        let (mut reader, mut asked, mut stream) = reader();
        reader.take(reader.regular(true));
        let first = snapshot_asked(&mut asked);
        reader.answered(Line::Snapshot(first), shot_replies((100, 30)));
        let _ = (next(&mut stream).await, next(&mut stream).await);

        // Off screen, nothing missed: back on, only the size goes.
        reader.control(StreamInput::View {
            visible: false,
            cols: 100,
            rows: 30,
        });
        reader.control(StreamInput::View {
            visible: true,
            cols: 120,
            rows: 40,
        });
        assert!(matches!(asked.try_recv(), Ok(Ask::Resize((120, 40)))));
        reader.answered(Line::Resize, vec![reply(&[]), reply(&["120x40"])]);

        // Off screen while the program writes: dropped, then a snapshot.
        reader.control(StreamInput::View {
            visible: false,
            cols: 120,
            rows: 40,
        });
        assert_eq!(reader.relay.output(b"%0", b"while away".to_vec()), None);
        reader.control(StreamInput::View {
            visible: true,
            cols: 120,
            rows: 40,
        });
        let back = snapshot_asked(&mut asked);
        assert_eq!((back.size, back.screen_first), ((120, 40), true));
        assert_eq!(
            reader.relay.output(b"%0", b"in the snapshot".to_vec()),
            None
        );
    }

    #[tokio::test]
    async fn the_client_hears_of_another_size_before_a_snapshot_at_it() {
        let (mut reader, mut asked, mut stream) = reader();
        reader.take(reader.regular(true));
        let first = snapshot_asked(&mut asked);
        // Another client sized the window: the snapshot came at its size.
        reader.answered(Line::Snapshot(first), shot_replies((80, 24)));
        assert_eq!(
            next(&mut stream).await,
            StreamOutput::Window(Some((80, 24)))
        );
        assert!(matches!(next(&mut stream).await, StreamOutput::Data(_)));
        let _ = next(&mut stream).await;

        // While our own resize is in flight, tmux's layout notices wait.
        reader.control(StreamInput::Resize { cols: 90, rows: 25 });
        reader.window = Some((80, 24));
        reader.tell();
        assert!(matches!(asked.try_recv(), Ok(Ask::Resize((90, 25)))));
        // It settled: the window is the client's now.
        reader.answered(Line::Resize, vec![reply(&[]), reply(&["90x25"])]);
        assert_eq!(next(&mut stream).await, StreamOutput::Window(None));
    }

    #[test]
    fn layout_notices_carry_the_window_size() {
        let mut p = Parser::default();
        assert_eq!(
            p.line(b"%layout-change @0 b25d,80x24,0,0,0 b25d,80x24,0,0,0 *"),
            Some(Event::Layout { size: (80, 24) })
        );
    }

    #[test]
    fn renders_the_cursor_the_program_set() {
        let mut snap = snapshot(false);
        snap.state.cursor_style = Some(2);
        snap.state.cursor_colour = Some("#ff8800".into());
        let out = String::from_utf8(render(&snap, take((20, 3), 0, false)).0).unwrap();
        assert!(out.contains("\x1b[2 q\x1b]12;#ff8800\x07"), "{out:?}");
    }

    #[test]
    fn snapshots_say_when_the_session_holds_more() {
        let mut snap = snapshot(false);
        snap.state.history = 9000;
        let (_, lines, more) = render(&snap, take((20, 3), 5000, false));
        assert_eq!((lines, more), (2, true));
    }
}
