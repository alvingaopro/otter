//! tmux's side of a stream attach (D-118): a control-mode client (`tmux -C
//! attach-session`) instead of a terminal client. tmux then sends the pane's
//! own output (`%output`), which the client's terminal interprets itself, so
//! the client keeps the history and scrolls it without asking the host.
//!
//! The client's terminal is rebuilt from a snapshot on attach, and again when
//! it falls too far behind. One command line takes it: tmux runs a line's
//! commands without reading the pane in between, so output that came before
//! the snapshot's replies is in it and output after them is new. Each command
//! gets one reply, and an error drops the rest of its line, so replies are
//! matched to the lines sent by counting.
//!
//! Input goes back with `send-keys -H`, byte for byte. A bracketed paste goes
//! through a paste buffer instead, so tmux, which knows whether the program
//! asked for bracketed paste, adds the brackets or not. The client's terminal
//! can't be told that on attach, so every snapshot turns bracketing on.

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

use super::{EXIT_MARKER, OLD_EXIT_MARKER};
use crate::backend::{Flow, Stream, StreamInput};

/// Starts the reply that carries a snapshot's state.
const MARKER: &str = "otter-snapshot";
/// The pane's state, in the order [`State::parse`] reads it.
const STATE_FORMAT: &str = "#{pane_id} #{pane_width} #{pane_height} #{history_size} \
    #{alternate_on} #{alternate_saved_x} #{alternate_saved_y} #{cursor_x} #{cursor_y} \
    #{cursor_flag} #{keypad_cursor_flag} #{keypad_flag} #{wrap_flag} #{insert_flag} \
    #{origin_flag} #{scroll_region_upper} #{scroll_region_lower} #{mouse_standard_flag} \
    #{mouse_button_flag} #{mouse_all_flag} #{mouse_utf8_flag} #{mouse_sgr_flag}";
/// Commands in a snapshot's line (see [`snapshot_line`]).
const SNAPSHOT_REPLIES: usize = 5;
/// Output waiting for the client beyond this is dropped for a fresh snapshot.
const BEHIND: usize = 2 << 20;
/// Snapshots after falling behind come at least this far apart, so a client
/// slower than the program sees a screen, not a flicker of them.
const RESYNC_GAP: Duration = Duration::from_millis(500);
/// History in a snapshot, at most: all of it arrives before the client's
/// terminal shows any (synchronized output holds it, for up to a second).
const HISTORY_BYTES: usize = 256 << 10;
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
    let (ask_tx, ask_rx) = mpsc::unbounded_channel();
    let (sent_tx, sent_rx) = mpsc::unbounded_channel();
    let writer = Writer {
        size: size(cols, rows),
        history,
        input: Input::new(format!(
            "otter-paste-{}",
            PASTES.fetch_add(1, Ordering::Relaxed)
        )),
    };
    tokio::spawn(writer.run(stdin, in_rx, ask_rx, sent_tx));
    tokio::spawn(read(
        child,
        stdout,
        Relay::new(history),
        flow.clone(),
        ask_tx,
        sent_rx,
    ));
    Ok(Stream::new(flow, in_tx))
}

/// A line sent to tmux: how many replies it gets, and for a snapshot, the
/// size it was taken at.
struct Sent {
    replies: usize,
    snapshot: Option<(u16, u16)>,
}

/// Writes tmux commands: the client's input and size, and snapshots when
/// the reader asks.
struct Writer {
    size: (u16, u16),
    history: u32,
    input: Input,
}

impl Writer {
    async fn run(
        mut self,
        mut stdin: ChildStdin,
        mut input: mpsc::UnboundedReceiver<StreamInput>,
        mut asks: mpsc::UnboundedReceiver<()>,
        sent: mpsc::UnboundedSender<Sent>,
    ) {
        loop {
            let lines: Vec<(String, Sent)> = tokio::select! {
                input = input.recv() => match input {
                    Some(StreamInput::Data(data)) => self
                        .input
                        .commands(&data)
                        .into_iter()
                        .map(|line| (line, Sent { replies: 1, snapshot: None }))
                        .collect(),
                    Some(StreamInput::Resize { cols, rows }) => {
                        self.size = size(cols, rows);
                        let (cols, rows) = self.size;
                        vec![(
                            format!("refresh-client -C {cols}x{rows}"),
                            Sent { replies: 1, snapshot: None },
                        )]
                    }
                    // Detached: closing stdin ends the control client, and
                    // nothing reaches the process.
                    None => return,
                },
                // At the size the client has now, so a resize can't overtake it.
                Some(()) = asks.recv() => vec![(
                    snapshot_line(self.size, self.history),
                    Sent { replies: SNAPSHOT_REPLIES, snapshot: Some(self.size) },
                )],
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
/// the window is that size for the commands after it.
fn snapshot_line((cols, rows): (u16, u16), history: u32) -> String {
    format!(
        "refresh-client -C {cols}x{rows} ; display-message -p '{MARKER} {STATE_FORMAT}' ; \
         capture-pane -p -e -J -S -{history} -E -1 ; capture-pane -p -e -N ; \
         capture-pane -p -e -N -a -q",
        history = history.max(1),
    )
}

/// tmux's replies and the pane's output → what the client shows.
async fn read(
    mut child: Child,
    stdout: ChildStdout,
    mut relay: Relay,
    flow: Arc<Flow>,
    asks: mpsc::UnboundedSender<()>,
    mut sent: mpsc::UnboundedReceiver<Sent>,
) {
    let _ = asks.send(());
    let mut asked = Instant::now();
    let mut parser = Parser::default();
    let mut pending: VecDeque<(Sent, Vec<Reply>)> = VecDeque::new();
    // Fell behind: a snapshot once the client has caught up, and not before
    // `resync_at`.
    let mut resync = false;
    let mut resync_at: Option<Instant> = None;
    let mut stdout = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        tokio::select! {
            read = stdout.read_until(b'\n', &mut line) => {
                if !matches!(read, Ok(n) if n > 0) {
                    break;
                }
            }
            // `line` keeps what was read of a line if these win.
            more = flow.client(resync) => {
                if !more {
                    break;
                }
                resync = false;
                resync_at = Some(Instant::now().max(asked + RESYNC_GAP));
                continue;
            }
            () = async { tokio::time::sleep_until(resync_at.unwrap_or_else(Instant::now)).await },
                if resync_at.is_some() =>
            {
                resync_at = None;
                asked = Instant::now();
                let _ = asks.send(());
                continue;
            }
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let event = parser.line(&line);
        line.clear();
        let data = match event {
            Some(Event::Output { pane, data }) => match relay.output(&pane, data) {
                Some(data) if flow.bytes() + data.len() > BEHIND && relay.fell_behind() => {
                    // Not taken in time: a snapshot replaces all of it.
                    flow.clear();
                    resync = true;
                    continue;
                }
                Some(data) => data,
                None => continue,
            },
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
                let Some((record, replies)) = pending.pop_front() else {
                    continue;
                };
                match record
                    .snapshot
                    .and_then(|asked| relay.snapshot(replies, asked))
                {
                    Some(data) => data,
                    None => continue,
                }
            }
            None => continue,
        };
        if !flow.push(data) {
            break;
        }
    }
    flow.done();
    let _ = child.start_kill();
    let _ = child.wait().await;
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

/// One line of control-mode output, read.
#[derive(Debug, PartialEq)]
enum Event {
    /// The reply to one of this client's commands, `ok` unless it failed.
    Reply { ok: bool, lines: Vec<Vec<u8>> },
    /// The pane's own output.
    Output { pane: Vec<u8>, data: Vec<u8> },
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
        let f: Vec<&str> = line.strip_prefix(MARKER)?.split_whitespace().collect();
        if f.len() != 22 {
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

enum Phase {
    /// Output is in the snapshot being taken; skip it until the snapshot.
    Waiting {
        first: bool,
    },
    Streaming,
}

/// What the client should see, from what tmux sends.
struct Relay {
    phase: Phase,
    pane: Vec<u8>,
    history: u32,
}

impl Relay {
    fn new(history: u32) -> Self {
        Relay {
            phase: Phase::Waiting { first: true },
            pane: Vec::new(),
            history,
        }
    }

    /// A session has one pane; its id is known unless its state wasn't.
    fn output(&self, pane: &[u8], data: Vec<u8>) -> Option<Vec<u8>> {
        (matches!(self.phase, Phase::Streaming) && (pane == self.pane || self.pane.is_empty()))
            .then_some(data)
    }

    /// The replies to a snapshot's line, taken at `asked`: what rebuilds the
    /// client's terminal. A failed command loses its reply and the ones after
    /// it; what came is still shown.
    fn snapshot(&mut self, replies: Vec<Reply>, asked: (u16, u16)) -> Option<Vec<u8>> {
        let Phase::Waiting { first } = self.phase else {
            return None;
        };
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
                State::fallback(asked)
            });
        if (state.width, state.height) != asked {
            // Another client sized the window (window-size latest).
            tracing::debug!(?asked, got = ?(state.width, state.height),
                "stream attach: the window has another size");
        }
        let snapshot = Snapshot {
            state,
            history: part(2),
            screen: part(3),
            saved: part(4),
        };
        self.pane = snapshot.state.pane.clone();
        self.phase = Phase::Streaming;
        Some(render(&snapshot, self.history > 0, first))
    }

    /// The client fell behind: skip output until a fresh snapshot, if one
    /// isn't already coming.
    fn fell_behind(&mut self) -> bool {
        let streaming = matches!(self.phase, Phase::Streaming);
        if streaming {
            self.phase = Phase::Waiting { first: false };
        }
        streaming
    }
}

/// Bytes that make the client's terminal show `snap`. `first`: the terminal
/// is new, so the screen is drawn at once, before the history.
///
/// tmux's captures write a style once and it holds, across rows, until it
/// changes; each capture starts from the default style. A line feed that
/// scrolls fills the new line with the current background, so rows are
/// separated with the style reset and set again after.
fn render(snap: &Snapshot, history: bool, first: bool) -> Vec<u8> {
    let s = &snap.state;
    let height = usize::from(s.height);
    let shown = rows(&snap.screen, height);
    let mut out = Vec::new();
    if first {
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        out.extend_from_slice(LINE_DRAWING);
        draw(&mut out, &shown);
        cup(&mut out, s.cursor_y, s.cursor_x);
    }
    // CAN ends whatever sequence was cut off, RIS starts over, and what
    // follows shows only once it's all there.
    out.extend_from_slice(b"\x18\x1bc\x1b[?2026h");
    out.extend_from_slice(LINE_DRAWING);
    if history && s.history > 0 {
        let (dropped, kept) = budget(&snap.history, HISTORY_BYTES);
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
    let top = if s.origin { s.region_top } else { 0 };
    cup(&mut out, s.cursor_y.saturating_sub(top), s.cursor_x);
    out.extend_from_slice(b"\x1b[?2026l");
    out
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
            "{MARKER} %3 20 3 2 {} 4 1 5 2 1 1 0 1 0 0 0 2 0 0 1 0 1",
            u8::from(alternate)
        )
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
        let line =
            format!("{MARKER} %0 40 5 0 0 4294967295 4294967295 2 0 1 0 0 1 0 0 0 4 0 0 0 0 0");
        let state = State::parse(line.as_bytes()).expect("parsed");
        assert!(!state.alternate);
        assert_eq!((state.cursor_x, state.region_bottom), (2, 4));
        assert!(State::parse(b"otter-snapshot %0 40").is_none());
    }

    #[test]
    fn output_counts_only_after_the_snapshot() {
        let mut relay = Relay::new(10);
        assert!(
            snapshot_line((20, 3), 10)
                .starts_with("refresh-client -C 20x3 ; display-message -p 'otter-snapshot ")
        );
        assert_eq!(
            snapshot_line((20, 3), 10).matches(" ; ").count() + 1,
            SNAPSHOT_REPLIES
        );
        assert_eq!(relay.output(b"%3", b"before".to_vec()), None);
        let shown = relay
            .snapshot(
                replies(&state_line(false), &["old line"], &["$ ls", "a  b"]),
                (20, 3),
            )
            .unwrap();
        let shown = String::from_utf8(shown).unwrap();
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
        let mut relay = Relay::new(10);
        let shown = relay
            .snapshot(
                vec![Reply {
                    ok: false,
                    lines: vec![],
                }],
                (20, 2),
            )
            .unwrap();
        assert!(shown.ends_with(b"\x1b[?2026l"));
        assert_eq!(
            relay.output(b"%0", b"live".to_vec()),
            Some(b"live".to_vec())
        );

        let mut relay = Relay::new(10);
        let shown = relay
            .snapshot(
                replies("otter-snapshot %0 something new", &["old"], &["shown"]),
                (20, 2),
            )
            .unwrap();
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
        let mut relay = Relay::new(10);
        let first = relay
            .snapshot(replies(&state_line(false), &[], &["screen"]), (20, 3))
            .unwrap();
        assert!(
            first.starts_with(b"\x1b[H\x1b[2J"),
            "the first draws at once"
        );
        assert!(relay.fell_behind());
        assert!(!relay.fell_behind(), "one snapshot at a time");
        assert_eq!(relay.output(b"%3", b"dropped".to_vec()), None);
        let again = relay
            .snapshot(replies(&state_line(false), &[], &["screen"]), (20, 3))
            .unwrap();
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

        let out = String::from_utf8(render(&snapshot(false), true, false)).unwrap();
        assert_eq!(
            out,
            "\x18\x1bc\x1b[?2026h\x1b)0\
             h1\x1b[0m\r\n\x1b[31mh2  \x1b[0m\r\n\
             one\x1b[0m\r\ntwo   \x1b[0m\r\n\x1b[0m\
             \x1b[?1h\x1b[?1003h\x1b[?1006h\x1b[?2004h\
             \x1b[3;6H\x1b[?2026l"
        );
        let without = String::from_utf8(render(&snapshot(false), false, false)).unwrap();
        assert!(!without.contains("h1"), "history wasn't asked for");
    }

    #[test]
    fn renders_the_alternate_screen_over_the_normal_one() {
        let out = String::from_utf8(render(&snapshot(true), true, true)).unwrap();
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
        let out = String::from_utf8(render(&snap, true, true)).unwrap();
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
        let out = String::from_utf8(render(&snap, false, false)).unwrap();
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
}
