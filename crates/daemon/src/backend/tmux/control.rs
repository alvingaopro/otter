//! tmux's side of a stream attach (D-118): a control-mode client (`tmux -C
//! attach-session`) instead of a terminal client. tmux then sends the pane's
//! own output (`%output`), which the client's terminal interprets itself, so
//! the client keeps the history and scrolls it without asking the host.
//!
//! The client's terminal is rebuilt from a snapshot on attach, and again when
//! it falls too far behind. One command line takes it: tmux runs a line's
//! commands without reading the pane in between, so output that came before
//! the snapshot's replies is in it and output after them is new.
//!
//! Input goes back with `send-keys -H`, byte for byte. A bracketed paste goes
//! through a paste buffer instead, so tmux, which knows whether the program
//! asked for bracketed paste, adds the brackets or not. The client's terminal
//! can't be told that on attach, so every snapshot turns bracketing on.

use std::fmt::Write as _;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

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
/// Output waiting for the client beyond this is dropped for a fresh snapshot.
const BEHIND: usize = 2 << 20;
/// History in a snapshot, at most: all of it arrives before the client's
/// terminal shows any (synchronized output holds it, for up to a second).
const HISTORY_BYTES: usize = 256 << 10;
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
    let (cols, rows) = size(cols, rows);
    let size = Arc::new(AtomicU32::new(pack(cols, rows)));
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    tokio::spawn(write(stdin, in_rx, cmd_rx, size.clone()));
    tokio::spawn(read(
        child,
        stdout,
        Relay::new(history),
        out_tx,
        cmd_tx,
        flow.clone(),
        size,
    ));
    Ok(Stream::new(out_rx, in_tx, flow))
}

/// The client's input and size → tmux commands.
async fn write(
    mut stdin: ChildStdin,
    mut input: mpsc::UnboundedReceiver<StreamInput>,
    mut commands: mpsc::UnboundedReceiver<String>,
    size: Arc<AtomicU32>,
) {
    let mut keys = Input::new(format!(
        "otter-paste-{}",
        PASTES.fetch_add(1, Ordering::Relaxed)
    ));
    loop {
        let lines = tokio::select! {
            input = input.recv() => match input {
                Some(StreamInput::Data(data)) => keys.commands(&data),
                Some(StreamInput::Resize { cols, rows }) => {
                    let (cols, rows) = self::size(cols, rows);
                    size.store(pack(cols, rows), Ordering::Relaxed);
                    vec![format!("refresh-client -C {cols}x{rows}")]
                }
                // Detached: closing stdin ends the control client, and
                // nothing reaches the process.
                None => return,
            },
            Some(line) = commands.recv() => vec![line],
        };
        for line in lines {
            // An empty line would detach the client.
            debug_assert!(!line.is_empty());
            if stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

/// tmux's replies and the pane's output → what the client shows.
async fn read(
    mut child: Child,
    stdout: ChildStdout,
    mut relay: Relay,
    out: mpsc::UnboundedSender<(u64, Vec<u8>)>,
    commands: mpsc::UnboundedSender<String>,
    flow: Arc<Flow>,
    size: Arc<AtomicU32>,
) {
    let _ = commands.send(relay.command(unpack(size.load(Ordering::Relaxed))));
    let mut stdout = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = tokio::select! {
            read = stdout.read_until(b'\n', &mut line) => read,
            // The stream was dropped: the attach is over, however it ended.
            () = out.closed() => break,
        };
        if !matches!(read, Ok(n) if n > 0) {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let data = match relay.line(&line) {
            Some(Relayed::Snapshot(data)) => data,
            Some(Relayed::Output(data)) => {
                if flow.queued.load(Ordering::Relaxed) > BEHIND && relay.fell_behind() {
                    // Skip what's queued; a snapshot replaces it.
                    flow.generation.fetch_add(1, Ordering::Relaxed);
                    let _ = commands.send(relay.command(unpack(size.load(Ordering::Relaxed))));
                    continue;
                }
                data
            }
            None => continue,
        };
        flow.queued.fetch_add(data.len(), Ordering::Relaxed);
        if out
            .send((flow.generation.load(Ordering::Relaxed), data))
            .is_err()
        {
            break;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Some clients report 0×0 when they can't tell; don't shrink the session's
/// window to nothing because of it.
fn size(cols: u16, rows: u16) -> (u16, u16) {
    if cols == 0 || rows == 0 {
        (80, 24)
    } else {
        (cols, rows)
    }
}

fn pack(cols: u16, rows: u16) -> u32 {
    (u32::from(cols) << 16) | u32::from(rows)
}

fn unpack(size: u32) -> (u16, u16) {
    ((size >> 16) as u16, size as u16)
}

/// One line of control-mode output, read.
#[derive(Debug, PartialEq)]
enum Event {
    /// A command's reply, `ok` unless it failed.
    Reply { ok: bool, lines: Vec<Vec<u8>> },
    /// The pane's own output.
    Output { pane: Vec<u8>, data: Vec<u8> },
}

/// Reads control-mode lines. A reply runs from `%begin` to the `%end` (or
/// `%error`) with the same arguments, so a line of pane text that happens
/// to start with `%end` can't end it.
#[derive(Default)]
struct Parser {
    reply: Option<(Vec<u8>, Vec<Vec<u8>>)>,
}

impl Parser {
    fn line(&mut self, line: &[u8]) -> Option<Event> {
        if let Some((guard, lines)) = &mut self.reply {
            for (end, ok) in [(&b"%end "[..], true), (b"%error ", false)] {
                if line.strip_prefix(end) == Some(guard.as_slice()) {
                    let (_, lines) = self.reply.take()?;
                    return Some(Event::Reply { ok, lines });
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

/// The pane's terminal state, from the snapshot's first reply.
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
            history: 1,
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
    /// Output is in the snapshot being taken; skip it until its first reply.
    Waiting {
        first: bool,
    },
    /// The snapshot's first reply came; the rest follow it directly.
    Taking {
        first: bool,
        state: State,
        parts: Vec<Vec<Vec<u8>>>,
    },
    Streaming,
}

enum Relayed {
    Snapshot(Vec<u8>),
    Output(Vec<u8>),
}

/// What the client should see, from what tmux sends.
struct Relay {
    parser: Parser,
    phase: Phase,
    pane: Vec<u8>,
    history: u32,
    asked: (u16, u16),
}

impl Relay {
    fn new(history: u32) -> Self {
        Relay {
            parser: Parser::default(),
            phase: Phase::Waiting { first: true },
            pane: Vec::new(),
            history,
            asked: (0, 0),
        }
    }

    /// The command line that takes a snapshot at the client's size.
    fn command(&mut self, (cols, rows): (u16, u16)) -> String {
        self.asked = (cols, rows);
        format!(
            "refresh-client -C {cols}x{rows} ; display-message -p '{MARKER} {STATE_FORMAT}' ; \
             capture-pane -p -e -J -S -{history} -E -1 ; capture-pane -p -e ; \
             capture-pane -p -e -a -q",
            history = self.history.max(1),
        )
    }

    fn line(&mut self, line: &[u8]) -> Option<Relayed> {
        match self.parser.line(line)? {
            // A session has one pane; its id is known unless the state wasn't.
            Event::Output { pane, data } => (matches!(self.phase, Phase::Streaming)
                && (pane == self.pane || self.pane.is_empty()))
            .then_some(Relayed::Output(data)),
            Event::Reply { ok, lines } => match &mut self.phase {
                Phase::Waiting { first } => {
                    // Other commands' replies (input, resizes) come and go.
                    let line = lines
                        .first()
                        .filter(|l| ok && l.starts_with(MARKER.as_bytes()))?;
                    let state = State::parse(line).unwrap_or_else(|| {
                        // Still show the screen and the history.
                        tracing::warn!(
                            line = %String::from_utf8_lossy(line),
                            "stream attach: unexpected pane state"
                        );
                        State::fallback(self.asked)
                    });
                    self.phase = Phase::Taking {
                        first: *first,
                        state,
                        parts: Vec::new(),
                    };
                    None
                }
                Phase::Taking {
                    first,
                    state,
                    parts,
                } => {
                    parts.push(if ok { lines } else { Vec::new() });
                    if parts.len() < 3 {
                        return None;
                    }
                    let mut parts = std::mem::take(parts).into_iter();
                    let snapshot = Snapshot {
                        state: state.clone(),
                        history: parts.next().unwrap_or_default(),
                        screen: parts.next().unwrap_or_default(),
                        saved: parts.next().unwrap_or_default(),
                    };
                    let first = *first;
                    if (snapshot.state.width, snapshot.state.height) != self.asked {
                        // Another client sized the window (window-size latest).
                        tracing::debug!(
                            asked = ?self.asked,
                            got = ?(snapshot.state.width, snapshot.state.height),
                            "stream attach: the window has another size"
                        );
                    }
                    self.pane = snapshot.state.pane.clone();
                    self.phase = Phase::Streaming;
                    Some(Relayed::Snapshot(render(&snapshot, first)))
                }
                Phase::Streaming => None,
            },
        }
    }

    /// The client fell behind: skip output until a fresh snapshot, if one
    /// isn't already coming. Then take it with [`Self::command`].
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
fn render(snap: &Snapshot, first: bool) -> Vec<u8> {
    let s = &snap.state;
    let height = usize::from(s.height);
    let shown = rows(&snap.screen, height);
    let mut out = Vec::new();
    if first {
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        draw(&mut out, &shown);
        cup(&mut out, s.cursor_y, s.cursor_x);
    }
    // CAN ends whatever sequence was cut off, RIS starts over, and what
    // follows shows only once it's all there.
    out.extend_from_slice(b"\x18\x1bc\x1b[?2026h");
    if s.history > 0 {
        // As captured: trailing blanks that wrapped a line keep wrapping it.
        for row in budget(&snap.history, HISTORY_BYTES) {
            out.extend_from_slice(row);
            out.extend_from_slice(b"\x1b[0m\r\n");
        }
    }
    let normal = if s.alternate {
        rows(&snap.saved, height)
    } else {
        shown.clone()
    };
    for (i, row) in normal.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(row);
        out.extend_from_slice(b"\x1b[0m");
    }
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

/// `lines` as exactly `height` rows of the screen, cleaned.
fn rows(lines: &[Vec<u8>], height: usize) -> Vec<Vec<u8>> {
    let mut rows: Vec<Vec<u8>> = lines.iter().take(height).map(|l| clean(l)).collect();
    rows.resize(height, Vec::new());
    rows
}

/// Each row at its place, so nothing scrolls.
fn draw(out: &mut Vec<u8>, rows: &[Vec<u8>]) {
    for (i, row) in rows.iter().enumerate() {
        cup(out, i as u16, 0);
        out.extend_from_slice(row);
        out.extend_from_slice(b"\x1b[0m");
    }
}

fn cup(out: &mut Vec<u8>, row: u16, col: u16) {
    let _ = write!(Bytes(out), "\x1b[{};{}H", row + 1, col + 1);
}

/// The latest history lines that fit in `max` bytes.
fn budget(lines: &[Vec<u8>], max: usize) -> &[Vec<u8>] {
    let mut total = 0;
    let start = lines
        .iter()
        .rposition(|l| {
            total += l.len() + 6;
            total > max
        })
        .map_or(0, |i| i + 1);
    &lines[start..]
}

/// A captured row of the screen without trailing blanks (each row has a line
/// of its own), and without the marker tmux draws under an exited process.
fn clean(line: &[u8]) -> Vec<u8> {
    let plain = strip_csi(line);
    if plain.starts_with(EXIT_MARKER.as_bytes()) || plain.starts_with(OLD_EXIT_MARKER.as_bytes()) {
        return Vec::new();
    }
    let end = line.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1);
    line[..end].to_vec()
}

fn strip_csi(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        if line[i] == 0x1b && line.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < line.len() && !(0x40..=0x7e).contains(&line[i]) {
                i += 1;
            }
            i += 1;
        } else {
            out.push(line[i]);
            i += 1;
        }
    }
    out
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

/// Typed bytes, as they are.
fn keys(data: &[u8], out: &mut Vec<String>) {
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

    fn lines(text: &[&str]) -> Vec<Vec<u8>> {
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
                lines: lines(&["%end 1 6 1", "text"])
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

    /// Feeds `text` (lines) to `relay`; what it relayed, as text.
    fn feed(relay: &mut Relay, text: &[&str]) -> Vec<String> {
        text.iter()
            .filter_map(|l| relay.line(l.as_bytes()))
            .map(|r| match r {
                Relayed::Snapshot(d) => format!("snapshot {}", String::from_utf8_lossy(&d)),
                Relayed::Output(d) => format!("output {}", String::from_utf8_lossy(&d)),
            })
            .collect()
    }

    #[test]
    fn output_counts_only_after_the_snapshot() {
        let mut relay = Relay::new(10);
        let command = relay.command((20, 3));
        assert!(
            command.starts_with("refresh-client -C 20x3 ; display-message -p 'otter-snapshot ")
        );
        assert!(command.contains("capture-pane -p -e -J -S -10 -E -1"));
        let state = state_line(false);
        let got = feed(
            &mut relay,
            &[
                "%begin 1 1 0",
                "%end 1 1 0",
                "%output %3 before",
                "%begin 1 2 1",
                "%end 1 2 1",
                "%begin 1 3 1",
                &state,
                "%end 1 3 1",
                "%begin 1 4 1",
                "old line",
                "%end 1 4 1",
                "%begin 1 5 1",
                "$ ls",
                "a  b",
                "",
                "%end 1 5 1",
                "%begin 1 6 1",
                "%end 1 6 1",
                "%output %9 another pane",
                "%output %3 after",
            ],
        );
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].starts_with("snapshot "), "{got:?}");
        assert!(got[0].contains("old line"));
        assert!(got[0].contains("$ ls"));
        assert!(!got[0].contains("before"));
        assert_eq!(got[1], "output after");
    }

    #[test]
    fn falling_behind_skips_output_until_a_new_snapshot() {
        let mut relay = Relay::new(10);
        relay.command((20, 3));
        let state = state_line(false);
        let snapshot = [
            "%begin 1 3 1",
            state.as_str(),
            "%end 1 3 1",
            "%begin 1 4 1",
            "%end 1 4 1",
            "%begin 1 5 1",
            "screen",
            "%end 1 5 1",
            "%begin 1 6 1",
            "%end 1 6 1",
        ];
        let first = feed(&mut relay, &snapshot);
        assert!(
            first[0].contains("\x1b[H\x1b[2J"),
            "the first draws at once"
        );
        assert!(relay.fell_behind());
        assert!(!relay.fell_behind(), "one snapshot at a time");
        // Input's replies come and go while the snapshot is awaited.
        let got = feed(
            &mut relay,
            &["%output %3 dropped", "%begin 1 7 1", "%end 1 7 1"],
        );
        assert!(got.is_empty(), "{got:?}");
        let again = feed(&mut relay, &snapshot);
        assert_eq!(again.len(), 1);
        assert!(again[0].starts_with("snapshot \x18\x1bc"), "{again:?}");
        assert_eq!(feed(&mut relay, &["%output %3 live"]), ["output live"]);
    }

    fn snapshot(alternate: bool) -> Snapshot {
        Snapshot {
            state: State::parse(state_line(alternate).as_bytes()).unwrap(),
            history: lines(&["h1", "\x1b[31mh2\x1b[39m  "]),
            screen: lines(&["one", "two   ", "[process exited, status 0]"]),
            saved: lines(&["n1", "n2", "n3"]),
        }
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
    fn a_state_it_cannot_read_still_shows_the_screen() {
        let mut relay = Relay::new(10);
        relay.command((20, 2));
        let got = feed(
            &mut relay,
            &[
                "%begin 1 3 1",
                "otter-snapshot %0 something new",
                "%end 1 3 1",
                "%begin 1 4 1",
                "old",
                "%end 1 4 1",
                "%begin 1 5 1",
                "shown",
                "%end 1 5 1",
                "%begin 1 6 1",
                "%end 1 6 1",
                "%output %0 live",
            ],
        );
        assert!(
            got[0].contains("old") && got[0].contains("shown"),
            "{got:?}"
        );
        assert_eq!(got[1], "output live");
    }

    #[test]
    fn renders_history_screen_cursor_and_modes() {
        let state = &snapshot(false).state;
        assert_eq!((state.width, state.height, state.history), (20, 3, 2));
        assert_eq!((state.cursor_x, state.cursor_y), (5, 2));
        assert!(state.cursor_visible && state.cursor_keys && !state.keypad);
        assert_eq!((state.region_top, state.region_bottom), (0, 2));
        assert!(state.mouse_all && state.mouse_sgr && !state.mouse_standard);

        let out = String::from_utf8(render(&snapshot(false), false)).unwrap();
        assert_eq!(
            out,
            "\x18\x1bc\x1b[?2026h\
             h1\x1b[0m\r\n\x1b[31mh2\x1b[39m  \x1b[0m\r\n\
             one\x1b[0m\r\ntwo\x1b[0m\r\n\x1b[0m\
             \x1b[?1h\x1b[?1003h\x1b[?1006h\x1b[?2004h\
             \x1b[3;6H\x1b[?2026l"
        );
    }

    #[test]
    fn renders_the_alternate_screen_over_the_normal_one() {
        let out = String::from_utf8(render(&snapshot(true), true)).unwrap();
        let (first, rest) = out.split_once("\x18\x1bc\x1b[?2026h").unwrap();
        assert_eq!(
            first,
            "\x1b[H\x1b[2J\x1b[1;1Hone\x1b[0m\x1b[2;1Htwo\x1b[0m\x1b[3;1H\x1b[0m\x1b[3;6H"
        );
        assert!(rest.starts_with(
            "h1\x1b[0m\r\n\x1b[31mh2\x1b[39m  \x1b[0m\r\nn1\x1b[0m\r\nn2\x1b[0m\r\nn3\x1b[0m"
        ));
        assert!(rest.contains(
            "\x1b[2;5H\x1b[?1049h\x1b[1;1Hone\x1b[0m\x1b[2;1Htwo\x1b[0m\x1b[3;1H\x1b[0m"
        ));
    }

    #[test]
    fn history_is_kept_to_a_budget_from_the_end() {
        let history = lines(&["aaaa", "bbbb", "cccc"]);
        assert_eq!(budget(&history, 100).len(), 3);
        assert_eq!(budget(&history, 20), &history[1..]);
        assert!(budget(&history, 5).is_empty());
    }

    #[test]
    fn keys_go_as_bytes_and_pastes_through_a_buffer() {
        let mut input = Input::new("otter-paste-1".into());
        assert_eq!(
            input.commands(b"a\x1b[A\x03"),
            ["send-keys -H 61 1b 5b 41 03"]
        );
        assert!(input.commands(b"").is_empty(), "never an empty line");
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
}
