//! Terminal queries taken out of a program's output (D-119), for a stream
//! whose client is the user's own terminal (`otter attach --scrollback`).
//!
//! A program asks its terminal things: where the cursor is, what it can do,
//! its colours. The session's terminal (the backend's) has answered already;
//! the user's terminal would answer again, and its answers would arrive in
//! the session as typing. So queries are dropped on the way, and everything
//! else passes as it is, also when a sequence is split across chunks.
//!
//! Only 7-bit sequences (`ESC [` and so on) are read: no program writes the
//! 8-bit C1 forms, and UTF-8 text is never mistaken for them.

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// Answerback: some terminals reply with a string.
const ENQ: u8 = 0x05;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
/// A CSI longer than this isn't a query; it passes.
const MAX_CSI: usize = 64;
/// A string sequence (OSC, DCS) is a query or not by its start; this much
/// is enough to tell.
const STRING_HEAD: usize = 32;
/// A kitty picture's control data ends at its first `;`, and asks (`a=q`)
/// anywhere in it: up to this much is held to tell.
const APC_HEAD: usize = 256;

#[derive(Default)]
pub struct Queries {
    state: State,
    /// The sequence so far, while it can still be a query.
    held: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    /// OSC, DCS or APC, held while it may be a query.
    String,
    /// …and its ESC, which may start the ST that ends it.
    StringEsc,
    /// A string that's no query: passed through to its end.
    Pass,
    PassEsc,
    /// A string that's a query: dropped to its end.
    Drop,
    DropEsc,
}

impl Queries {
    /// `data` without the queries in it; a sequence cut off at its end is
    /// held for the next chunk.
    pub fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            self.byte(b, &mut out);
        }
        out
    }

    fn byte(&mut self, b: u8, out: &mut Vec<u8>) {
        use State::*;
        // CAN and SUB end any sequence; the terminal sees them too.
        if matches!(b, CAN | SUB) && self.state != Ground {
            if !matches!(self.state, Drop | DropEsc) {
                out.append(&mut self.held);
            }
            self.held.clear();
            out.push(b);
            self.state = Ground;
            return;
        }
        match self.state {
            Ground => match b {
                ESC => self.hold(b, Esc),
                ENQ => {}
                _ => out.push(b),
            },
            Esc => match b {
                b'[' => self.hold(b, Csi),
                b']' | b'P' | b'_' => self.hold(b, String),
                // DECID, an old way of asking what the terminal is.
                b'Z' => self.drop_held(),
                ESC => {
                    out.append(&mut self.held);
                    self.hold(b, Esc);
                }
                _ => {
                    self.held.push(b);
                    self.pass_held(out);
                }
            },
            Csi => {
                if b == ESC {
                    out.append(&mut self.held);
                    self.hold(b, Esc);
                } else if (0x40..=0x7e).contains(&b) {
                    self.held.push(b);
                    if csi_query(&self.held[2..]) {
                        self.drop_held();
                    } else {
                        self.pass_held(out);
                    }
                } else if self.held.len() >= MAX_CSI {
                    self.held.push(b);
                    self.pass_held(out);
                } else {
                    self.held.push(b);
                }
            }
            String | StringEsc => {
                let ends = (self.state == StringEsc && b == b'\\') || b == BEL;
                self.held.push(b);
                if ends {
                    if string_query(&self.held, true) {
                        self.drop_held();
                    } else {
                        self.pass_held(out);
                    }
                    return;
                }
                self.state = if b == ESC { StringEsc } else { String };
                // Long enough to tell: a query is dropped to its end, and
                // anything else (a picture, a clipboard write) streams on.
                if telling(&self.held) {
                    if string_query(&self.held, false) {
                        self.held.clear();
                        self.state = if b == ESC { DropEsc } else { Drop };
                    } else {
                        out.append(&mut self.held);
                        self.state = if b == ESC { PassEsc } else { Pass };
                    }
                }
            }
            Pass | PassEsc => {
                out.push(b);
                self.state = string_next(self.state == PassEsc, b, Pass, PassEsc);
            }
            Drop | DropEsc => {
                self.state = string_next(self.state == DropEsc, b, Drop, DropEsc);
            }
        }
    }

    fn hold(&mut self, b: u8, state: State) {
        self.held.push(b);
        self.state = state;
    }

    fn drop_held(&mut self) {
        self.held.clear();
        self.state = State::Ground;
    }

    fn pass_held(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
        self.state = State::Ground;
    }
}

/// Whether enough of a string sequence is held to tell if it's a query.
fn telling(seq: &[u8]) -> bool {
    if seq[1] == b'_' {
        seq.len() >= APC_HEAD || seq[2..].contains(&b';')
    } else {
        seq.len() >= STRING_HEAD
    }
}

/// The state after `b` in a string being passed or dropped: ST (ESC `\`) or
/// BEL ends it.
fn string_next(after_esc: bool, b: u8, string: State, esc: State) -> State {
    if b == BEL || (after_esc && b == b'\\') {
        State::Ground
    } else if b == ESC {
        esc
    } else {
        string
    }
}

/// Whether a CSI (what follows `ESC [`, final byte included) asks the
/// terminal something.
fn csi_query(body: &[u8]) -> bool {
    let Some((&last, rest)) = body.split_last() else {
        return false;
    };
    let prefix = rest.first().copied().filter(|b| b"?>=<".contains(b));
    let params = &rest[usize::from(prefix.is_some())..];
    let intermediates: Vec<u8> = params
        .iter()
        .copied()
        .filter(|b| (0x20..=0x2f).contains(b))
        .collect();
    let first: Option<u32> = params
        .split(|&b| b == b';' || b == b':')
        .next()
        .and_then(|p| std::str::from_utf8(p).ok())
        .and_then(|p| p.parse().ok());
    match (prefix, intermediates.as_slice(), last) {
        // Device attributes, primary, secondary, tertiary.
        (None | Some(b'>') | Some(b'='), [], b'c') => true,
        // Status and cursor position reports (not `CSI > n`, which sets).
        (None | Some(b'?'), [], b'n') => true,
        // Whether a mode is set.
        (None | Some(b'?'), [b'$'], b'p') => true,
        // The terminal's name and version.
        (Some(b'>'), [], b'q') => true,
        // The keyboard protocol in use (kitty).
        (Some(b'?'), [], b'u') => true,
        // Terminal parameters.
        (None, [], b'x') => true,
        // Sixel and ReGIS graphics attributes (every form is answered).
        (Some(b'?'), [], b'S') => params.contains(&b';'),
        // Which key modifiers are reported how.
        (Some(b'?'), [], b'm') => true,
        // A checksum of an area, the presentation state.
        (None, [b'*'], b'y') | (None, [b'$'], b'w') => true,
        // Window reports; other window operations act.
        (None, [], b't') => matches!(first, Some(11 | 13..=16 | 18..=21)),
        _ => false,
    }
}

/// Whether a string sequence (`ESC ]`, `ESC P` or `ESC _` and what follows)
/// asks the terminal something. `whole`: it's all here.
fn string_query(seq: &[u8], whole: bool) -> bool {
    let body = &seq[2..];
    let body = body
        .strip_suffix(b"\x1b\\")
        .or_else(|| body.strip_suffix(&[BEL]))
        .unwrap_or(body);
    match seq[1] {
        b']' => {
            let mut fields = body.split(|&b| b == b';');
            let code = fields.next().unwrap_or_default();
            match code {
                // Colours (palette, special, foreground…), the clipboard.
                b"4" | b"5" | b"10" | b"11" | b"12" | b"13" | b"14" | b"15" | b"16" | b"17"
                | b"18" | b"19" | b"52" => fields.any(|f| f == b"?"),
                // iTerm2's reports.
                b"1337" => body.starts_with(b"1337;Report"),
                _ => false,
            }
        }
        // Settings and capabilities (DECRQSS, XTGETTCAP).
        b'P' => body.starts_with(b"$q") || body.starts_with(b"+q"),
        // Whether pictures work (kitty): `a=q` in the control data, which
        // ends at the first `;`.
        b'_' => {
            let control = body.split(|&b| b == b';').next().unwrap_or_default();
            body.starts_with(b"G")
                && (whole || body.contains(&b';') || seq.len() >= APC_HEAD)
                && find(control, b"a=q")
        }
        _ => false,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(chunks: &[&[u8]]) -> Vec<u8> {
        let mut q = Queries::default();
        chunks.iter().flat_map(|c| q.filter(c)).collect()
    }

    #[test]
    fn queries_go_and_everything_else_stays() {
        let input: &[u8] = b"a\x1b[cb\x1b[>0cc\x1b[6nd\x1b[?6ne\x1b[?2026$pf\x1b[>qg\x1b[?uh\
            \x1b[18ti\x1b]11;?\x07j\x1b]10;?\x1b\\k\x1b]4;1;?\x07l\x1b]52;c;?\x07m\
            \x1bP$qm\x1b\\n\x1bP+q544e\x1b\\o\x05p\x1bZq";
        assert_eq!(filtered(&[input]), b"abcdefghijklmnopq");
        let kept: &[u8] = b"\x1b[31mred\x1b[0m\x1b[2J\x1b[?1049h\x1b[>4;1m\x1b[>4n\x1b[8;24;80t\
            \x1b[22;0t\x1b[2 q\x1b]0;title\x07\x1b]11;#000000\x07\x1b]52;c;aGk=\x07\
            \x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\\x1b[>1u\x1b[<u\x1bPq#0;2;0;0;0\x1b\\";
        assert_eq!(filtered(&[kept]), kept);
    }

    #[test]
    fn graphics_and_key_queries_go_too() {
        let input: &[u8] = b"a\x1b[?1;1;0Sb\x1b[?4mc\x1b[1;1;1;1;1;1*yd\x1b[1$we\x1b_Gi=31,s=100,v=100,t=d,f=24,m=0,a=q;AAAA\x1b\\f";
        assert_eq!(filtered(&[input]), b"abcdef");
        // Not queries: a sixel colour, a picture sent, scrolling up.
        let kept: &[u8] = b"\x1b[?1S\x1b[2S\x1b_Ga=T,f=100;iVBORw0KGgo=\x1b\\";
        assert_eq!(filtered(&[kept]), kept);
    }

    #[test]
    fn sequences_split_across_chunks() {
        assert_eq!(filtered(&[b"a\x1b", b"[", b"6", b"nb"]), b"ab");
        assert_eq!(filtered(&[b"\x1b]11", b";?\x1b", b"\\x"]), b"x");
        assert_eq!(filtered(&[b"\x1b[3", b"1mz"]), b"\x1b[31mz");
        assert_eq!(filtered(&[b"\x1b]0;ti", b"tle\x07"]), b"\x1b]0;title\x07");
    }

    #[test]
    fn long_strings_stream_through() {
        // A big clipboard write isn't held whole.
        let mut q = Queries::default();
        let mut start = b"\x1b]52;c;".to_vec();
        start.extend(std::iter::repeat_n(b'A', 100));
        let out = q.filter(&start);
        assert_eq!(out, start, "streamed as it comes");
        assert_eq!(q.filter(b"AA\x07after"), b"AA\x07after");
    }

    #[test]
    fn a_cancelled_sequence_passes_with_its_cancel() {
        assert_eq!(filtered(&[b"\x1b[12\x18x"]), b"\x1b[12\x18x");
        assert_eq!(filtered(&[b"\x1b\x1b[6nx"]), b"\x1bx");
    }
}
