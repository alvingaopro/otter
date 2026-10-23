//! What Slack sends down the socket, and what of it Otter takes (D-200):
//! pure functions, so the rules are tested without a connection.

use serde_json::Value;

/// One message from Slack's Socket Mode.
#[derive(Debug, PartialEq)]
pub enum Envelope {
    /// The connection is up.
    Hello,
    /// Slack is closing this connection (a refresh, a deploy): open another.
    Disconnect(String),
    /// An event, acked by its id before it is looked at.
    Event {
        envelope_id: String,
        event: Value,
    },
    /// A button pressed on one of Otter's messages (D-201), acked the same
    /// way.
    Interactive {
        envelope_id: String,
        payload: Value,
    },
    /// `/otter` (D-202): its ack carries the answer.
    Command {
        envelope_id: String,
        payload: Value,
    },
    /// Anything else Slack wants acked: not taken.
    Other {
        envelope_id: String,
    },
    Unknown,
}

pub fn envelope(text: &str) -> Envelope {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Envelope::Unknown;
    };
    let id = v["envelope_id"].as_str().map(String::from);
    match (v["type"].as_str(), id) {
        (Some("hello"), _) => Envelope::Hello,
        (Some("disconnect"), _) => {
            Envelope::Disconnect(v["reason"].as_str().unwrap_or("asked to").to_owned())
        }
        (Some("events_api"), Some(envelope_id)) => Envelope::Event {
            envelope_id,
            event: v["payload"]["event"].clone(),
        },
        (Some("interactive"), Some(envelope_id)) => Envelope::Interactive {
            envelope_id,
            payload: v["payload"].clone(),
        },
        (Some("slash_commands"), Some(envelope_id)) => Envelope::Command {
            envelope_id,
            payload: v["payload"].clone(),
        },
        (_, Some(envelope_id)) => Envelope::Other { envelope_id },
        _ => Envelope::Unknown,
    }
}

/// A button pressed on one of Otter's messages (D-201).
#[derive(Debug, PartialEq)]
pub struct Press {
    /// Who pressed it: anyone who sees the message can.
    pub user: String,
    /// The button's `action_id`: what to do.
    pub action: String,
    /// The button's `value`: on what.
    pub value: String,
    /// When: one press, one command.
    pub at: String,
    /// The message it is on: its thread is its task's.
    pub on: Origin,
    /// The block it is in: a form's question is `q:<id>` (D-202).
    pub block: String,
    /// What a select chose.
    pub picked: Vec<String>,
    /// What the message's selects and inputs hold now (`state.values`).
    pub state: Value,
}

/// The press in an interactive payload; `None` for anything else.
pub fn press(payload: &Value) -> Option<Press> {
    if payload["type"] != "block_actions" {
        return None;
    }
    let action = &payload["actions"][0];
    let channel = payload["container"]["channel_id"]
        .as_str()
        .or_else(|| payload["channel"]["id"].as_str())?;
    Some(Press {
        user: payload["user"]["id"].as_str()?.to_owned(),
        action: action["action_id"].as_str()?.to_owned(),
        value: action["value"].as_str().unwrap_or_default().to_owned(),
        at: action["action_ts"].as_str()?.to_owned(),
        on: Origin {
            channel: channel.to_owned(),
            thread: Some(payload["container"]["message_ts"].as_str()?.to_owned()),
        },
        block: action["block_id"].as_str().unwrap_or_default().to_owned(),
        picked: picked(action),
        state: payload["state"]["values"].clone(),
    })
}

/// What a select (one or several) chose.
fn picked(element: &Value) -> Vec<String> {
    if let Some(one) = element["selected_option"]["value"].as_str() {
        return vec![one.to_owned()];
    }
    element["selected_options"]
        .as_array()
        .map(|many| {
            many.iter()
                .filter_map(|o| o["value"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// What an input holds, trimmed.
fn typed(element: &Value) -> Vec<String> {
    element["value"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| vec![v.to_owned()])
        .unwrap_or_default()
}

/// A form's answers as its message holds them (`state.values`), by
/// question: several choices joined with ", ", as the app joins them.
pub fn form_answers(values: &Value) -> std::collections::BTreeMap<String, String> {
    let Some(blocks) = values.as_object() else {
        return Default::default();
    };
    blocks
        .iter()
        .filter_map(|(block, actions)| {
            let question = block.strip_prefix("q:")?;
            let element = actions.as_object()?.values().next()?;
            let chosen = match picked(element) {
                none if none.is_empty() => typed(element),
                some => some,
            };
            (!chosen.is_empty()).then(|| (question.to_owned(), chosen.join(", ")))
        })
        .collect()
}

/// `/otter` as the developer typed it (D-202).
#[derive(Debug, PartialEq)]
pub struct Command {
    /// Who typed it: only a listed member is answered.
    pub user: String,
    pub verb: Verb,
}

/// What `/otter` is asked to do.
#[derive(Debug, PartialEq)]
pub enum Verb {
    /// What needs the developer, and what is at work: `/otter`,
    /// `/otter status`.
    Status,
    /// A task, made and started from these words: `/otter new <words>`.
    New(String),
    /// What `/otter` does: `/otter help`, or anything it doesn't know.
    Help,
}

/// The command in a slash command's payload; `None` for another command.
pub fn command(payload: &Value) -> Option<Command> {
    if payload["command"] != "/otter" {
        return None;
    }
    let text = plain(payload["text"].as_str().unwrap_or_default());
    let text = text.trim();
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let verb = match word.to_lowercase().as_str() {
        "" | "status" => Verb::Status,
        "new" if !rest.trim().is_empty() => Verb::New(rest.trim().to_owned()),
        _ => Verb::Help,
    };
    Some(Command {
        user: payload["user_id"].as_str()?.to_owned(),
        verb,
    })
}

/// A direct message from the developer, as Otter takes it.
#[derive(Debug, PartialEq)]
pub struct Inbound {
    pub to: Origin,
    pub ts: String,
    pub text: String,
    /// The files that came with it (D-202): each a picture to fetch, by the
    /// addresses to try in turn, or none to try (not a picture).
    pub files: Vec<Vec<String>>,
}

/// Where a message came from in Slack, and so where Otter answers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub channel: String,
    /// The thread it was written in, if any: the answer goes there too.
    pub thread: Option<String>,
}

/// Why a message isn't taken.
#[derive(Debug, PartialEq)]
pub enum Skip {
    NotAMessage,
    NotDirect,
    /// A bot's (Otter's own answers among them).
    FromABot,
    /// An edit, a deletion, a notice: what the developer wrote is taken once.
    NotWritten,
    /// From a Slack member Settings doesn't list.
    NotListed,
    Empty,
}

/// The developer's direct message in `event`, or why it isn't one: only a
/// message written in a direct message to the app, by one of `users`.
pub fn inbound(event: &Value, users: &[String]) -> Result<Inbound, Skip> {
    if event["type"] != "message" {
        return Err(Skip::NotAMessage);
    }
    if event["channel_type"] != "im" {
        return Err(Skip::NotDirect);
    }
    if !event["bot_id"].is_null() || event["subtype"] == "bot_message" {
        return Err(Skip::FromABot);
    }
    // A picture's message is written too; its words are taken (D-200).
    match event["subtype"].as_str() {
        None | Some("file_share") => {}
        Some(_) => return Err(Skip::NotWritten),
    }
    let user = event["user"].as_str().unwrap_or_default();
    if !users.iter().any(|u| u == user) {
        return Err(Skip::NotListed);
    }
    let (Some(channel), Some(ts)) = (event["channel"].as_str(), event["ts"].as_str()) else {
        return Err(Skip::NotAMessage);
    };
    let text = plain(event["text"].as_str().unwrap_or_default());
    let files: Vec<Vec<String>> = event["files"]
        .as_array()
        .map(|files| files.iter().map(picture_addresses).collect())
        .unwrap_or_default();
    if text.trim().is_empty() && files.is_empty() {
        return Err(Skip::Empty);
    }
    Ok(Inbound {
        to: Origin {
            channel: channel.to_owned(),
            thread: event["thread_ts"].as_str().map(String::from),
        },
        ts: ts.to_owned(),
        text: text.trim().to_owned(),
        files,
    })
}

/// Where a file in a message can be fetched as a picture Otter takes, best
/// first: the file itself while it fits, then Slack's thumbnails, largest
/// first. None for a file that isn't a picture.
fn picture_addresses(file: &Value) -> Vec<String> {
    if !file["mimetype"]
        .as_str()
        .is_some_and(|m| m.starts_with("image/"))
    {
        return vec![];
    }
    let mut out = Vec::new();
    let fits = file["size"]
        .as_u64()
        .is_some_and(|n| n as usize <= otter_protocol::task::MAX_IMAGE_BYTES);
    if fits && let Some(u) = file["url_private_download"].as_str() {
        out.push(u.to_owned());
    }
    for size in ["1024", "960", "800", "720", "480", "360"] {
        if let Some(u) = file[format!("thumb_{size}")].as_str() {
            out.push(u.to_owned());
        }
    }
    out
}

/// A picture's type by its first bytes, among those Otter takes (D-171).
pub fn picture_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

impl Inbound {
    /// The command that says it in Otter's conversation: the same message
    /// again (Slack sends an event again until it is acked) adds nothing.
    /// It is also the message's `correlation_id`, which is how Otter's
    /// answers find their way back.
    pub fn command_id(&self) -> String {
        match &self.to.thread {
            Some(t) => format!("slack:{}:{}:{t}", self.to.channel, self.ts),
            None => format!("slack:{}:{}", self.to.channel, self.ts),
        }
    }
}

impl Origin {
    /// Where a message of the conversation came from in Slack, by its
    /// `correlation_id`; `None` for one written elsewhere.
    pub fn of(correlation_id: Option<&str>) -> Option<Origin> {
        let mut parts = correlation_id?.strip_prefix("slack:")?.split(':');
        let channel = parts.next().filter(|c| !c.is_empty())?;
        let _ts = parts.next()?;
        Some(Origin {
            channel: channel.to_owned(),
            thread: parts.next().map(String::from),
        })
    }
}

/// Slack's markup as the words the developer wrote: links as their address
/// (or their label and address), mentions as `@`, and `&amp;` `&lt;` `&gt;`
/// as the characters.
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let inner = &rest[open + 1..open + close];
        let (target, label) = match inner.split_once('|') {
            Some((t, l)) => (t, Some(l)),
            None => (inner, None),
        };
        let shown = if let Some(user) = target.strip_prefix('@') {
            format!("@{}", label.unwrap_or(user))
        } else if let Some(channel) = target.strip_prefix('#') {
            format!("#{}", label.unwrap_or(channel))
        } else if let Some(special) = target.strip_prefix('!') {
            // `<!subteam^S1|@eng>` is labelled with its `@` already.
            let name = label.unwrap_or(special);
            format!("@{}", name.trim_start_matches('@'))
        } else {
            let address = target.strip_prefix("mailto:").unwrap_or(target);
            match label {
                // What was typed without its scheme (`github.com/a/b`) is
                // the address itself.
                Some(l) if l != address && !address.ends_with(l) => format!("{l} ({address})"),
                _ => address.to_owned(),
            }
        };
        out.push_str(&shown);
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// `text` in pieces of at most `max` characters, cut at a blank line, else
/// at a line's end, else where it must.
pub fn pieces(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > max {
        let end = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        let head = &rest[..end];
        let cut = head
            .rfind("\n\n")
            .or_else(|| head.rfind('\n'))
            .filter(|&i| i > 0)
            .unwrap_or(end);
        out.push(rest[..cut].trim_end().to_owned());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn users() -> Vec<String> {
        vec!["U1".into()]
    }

    fn dm(extra: Value) -> Value {
        let mut e = json!({
            "type": "message", "channel_type": "im", "channel": "D1",
            "user": "U1", "ts": "1700000000.000100", "text": "hello",
        });
        for (k, v) in extra.as_object().unwrap() {
            e[k] = v.clone();
        }
        e
    }

    #[test]
    fn envelopes_are_read_by_their_type() {
        assert_eq!(
            envelope(r#"{"type":"hello","num_connections":1}"#),
            Envelope::Hello
        );
        assert_eq!(
            envelope(r#"{"type":"disconnect","reason":"refresh_requested"}"#),
            Envelope::Disconnect("refresh_requested".into())
        );
        assert_eq!(
            envelope(
                r#"{"type":"events_api","envelope_id":"e1","payload":{"event":{"type":"message"}}}"#
            ),
            Envelope::Event {
                envelope_id: "e1".into(),
                event: json!({"type": "message"})
            }
        );
        assert_eq!(
            envelope(r#"{"type":"app_rate_limited","envelope_id":"e2","payload":{}}"#),
            Envelope::Other {
                envelope_id: "e2".into()
            }
        );
        assert_eq!(
            envelope(
                r#"{"type":"interactive","envelope_id":"e3","payload":{"type":"block_actions"}}"#
            ),
            Envelope::Interactive {
                envelope_id: "e3".into(),
                payload: json!({"type": "block_actions"})
            }
        );
        assert_eq!(
            envelope(
                r#"{"type":"slash_commands","envelope_id":"e4","payload":{"command":"/otter"}}"#
            ),
            Envelope::Command {
                envelope_id: "e4".into(),
                payload: json!({"command": "/otter"})
            }
        );
        assert_eq!(envelope("not json"), Envelope::Unknown);
    }

    /// A form's answers (D-202): a select's choice, several joined, an
    /// input's words; what wasn't answered isn't there.
    #[test]
    fn a_forms_answers_are_read_from_its_message() {
        let values = json!({
            "q:db": {"pick": {"type": "static_select", "selected_option": {"value": "Postgres"}}},
            "q:langs": {"pick": {"type": "multi_static_select",
                "selected_options": [{"value": "Rust"}, {"value": "Go"}]}},
            "q:name": {"text": {"type": "plain_text_input", "value": "  exporter "}},
            "q:empty": {"text": {"type": "plain_text_input", "value": ""}},
            "q:none": {"pick": {"type": "static_select", "selected_option": null}},
            "other": {"x": {"value": "not a question"}},
        });
        assert_eq!(
            form_answers(&values),
            std::collections::BTreeMap::from([
                ("db".to_owned(), "Postgres".to_owned()),
                ("langs".to_owned(), "Rust, Go".to_owned()),
                ("name".to_owned(), "exporter".to_owned()),
            ])
        );
        let pick = json!({
            "type": "block_actions", "user": {"id": "U1"},
            "container": {"message_ts": "1.5", "channel_id": "D1"},
            "actions": [{"action_id": "pick", "block_id": "q:db", "action_ts": "2.0",
                "selected_option": {"value": "SQLite"}}],
        });
        let p = press(&pick).unwrap();
        assert_eq!(
            (p.block.as_str(), p.picked.as_slice()),
            ("q:db", &["SQLite".to_owned()][..])
        );
    }

    #[test]
    fn otter_is_told_what_to_do_in_a_word() {
        let cmd = |text: &str| {
            command(&json!({"command": "/otter", "text": text, "user_id": "U1"}))
                .unwrap()
                .verb
        };
        assert_eq!(cmd(""), Verb::Status);
        assert_eq!(cmd(" Status "), Verb::Status);
        assert_eq!(
            cmd("new  Add <https://x.dev|x.dev> export\nwith tests"),
            Verb::New("Add https://x.dev export\nwith tests".into())
        );
        assert_eq!(cmd("new"), Verb::Help);
        assert_eq!(cmd("help"), Verb::Help);
        assert_eq!(cmd("deploy it"), Verb::Help);
        assert_eq!(
            command(&json!({"command": "/other", "user_id": "U1"})),
            None
        );
    }

    #[test]
    fn a_press_says_who_pressed_what_and_where() {
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U1"},
            "container": {"type": "message", "message_ts": "1.5", "channel_id": "D1"},
            "actions": [{"action_id": "approve", "value": "{\"task\":\"ft_a\"}", "action_ts": "1.9"}],
        });
        assert_eq!(
            press(&payload),
            Some(Press {
                user: "U1".into(),
                action: "approve".into(),
                value: r#"{"task":"ft_a"}"#.into(),
                at: "1.9".into(),
                on: Origin {
                    channel: "D1".into(),
                    thread: Some("1.5".into())
                },
                block: String::new(),
                picked: vec![],
                state: Value::Null,
            })
        );
        assert_eq!(press(&json!({"type": "view_submission"})), None);
    }

    #[test]
    fn only_what_a_listed_member_writes_to_the_app_is_taken() {
        let m = inbound(&dm(json!({})), &users()).unwrap();
        assert_eq!(m.text, "hello");
        assert_eq!(m.command_id(), "slack:D1:1700000000.000100");
        assert_eq!(
            inbound(&dm(json!({"user": "U2"})), &users()),
            Err(Skip::NotListed)
        );
        assert_eq!(
            inbound(&dm(json!({"channel_type": "channel"})), &users()),
            Err(Skip::NotDirect)
        );
        assert_eq!(
            inbound(&dm(json!({"bot_id": "B1"})), &users()),
            Err(Skip::FromABot)
        );
        assert_eq!(
            inbound(&dm(json!({"subtype": "message_changed"})), &users()),
            Err(Skip::NotWritten)
        );
        assert_eq!(
            inbound(&dm(json!({"text": "  "})), &users()),
            Err(Skip::Empty)
        );
        assert_eq!(
            inbound(&json!({"type": "reaction_added"}), &users()),
            Err(Skip::NotAMessage)
        );
        // Words that came with a picture are taken.
        let pic = inbound(&dm(json!({"subtype": "file_share"})), &users()).unwrap();
        assert_eq!(pic.text, "hello");
    }

    /// Pictures (D-202): the file while it fits, then its thumbnails,
    /// largest first; a file that isn't a picture has nowhere to be fetched
    /// from; words may be left out when a file came.
    #[test]
    fn a_messages_pictures_are_fetched_best_first() {
        let max = otter_protocol::task::MAX_IMAGE_BYTES;
        let event = dm(json!({"text": "", "subtype": "file_share", "files": [
            {"mimetype": "image/png", "size": 1000, "url_private_download": "u/a.png", "thumb_360": "t/a360"},
            {"mimetype": "image/heic", "size": max + 1, "url_private_download": "u/b.heic",
             "thumb_480": "t/b480", "thumb_1024": "t/b1024"},
            {"mimetype": "application/pdf", "size": 10, "url_private_download": "u/c.pdf"},
        ]}));
        let m = inbound(&event, &users()).unwrap();
        assert_eq!(m.text, "");
        assert_eq!(
            m.files,
            vec![
                vec!["u/a.png".to_owned(), "t/a360".to_owned()],
                vec!["t/b1024".to_owned(), "t/b480".to_owned()],
                vec![],
            ]
        );
        assert_eq!(picture_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(picture_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(picture_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(picture_type(b"%PDF-1.7"), None);
    }

    #[test]
    fn an_answer_goes_where_the_message_came_from() {
        let m = inbound(&dm(json!({"thread_ts": "1699.5"})), &users()).unwrap();
        let id = m.command_id();
        assert_eq!(id, "slack:D1:1700000000.000100:1699.5");
        assert_eq!(
            Origin::of(Some(&id)),
            Some(Origin {
                channel: "D1".into(),
                thread: Some("1699.5".into())
            })
        );
        assert_eq!(
            Origin::of(Some("slack:D1:1.2")),
            Some(Origin {
                channel: "D1".into(),
                thread: None
            })
        );
        // Written in the app or the terminal: not Slack's.
        assert_eq!(Origin::of(Some("cmd-1")), None);
        assert_eq!(Origin::of(None), None);
    }

    #[test]
    fn slacks_markup_becomes_what_was_written() {
        assert_eq!(
            plain("fix <https://github.com/a/b/pull/3> &amp; ship"),
            "fix https://github.com/a/b/pull/3 & ship"
        );
        assert_eq!(
            plain("in <https://github.com/a/b|github.com/a/b>"),
            "in https://github.com/a/b"
        );
        assert_eq!(
            plain("see <https://x.dev/doc|the doc>"),
            "see the doc (https://x.dev/doc)"
        );
        assert_eq!(
            plain("ask <@U9> in <#C1|eng> <!here>"),
            "ask @U9 in #eng @here"
        );
        assert_eq!(plain("cc <!subteam^S1|@eng>"), "cc @eng");
        assert_eq!(plain("mail <mailto:a@b.c|a@b.c>"), "mail a@b.c");
        assert_eq!(plain("a &lt;b&gt; c <unclosed"), "a <b> c <unclosed");
    }

    #[test]
    fn long_answers_are_cut_where_a_paragraph_ends() {
        assert_eq!(pieces("short", 10), vec!["short"]);
        assert_eq!(pieces("aaaa\n\nbbbb\ncccc", 12), vec!["aaaa", "bbbb\ncccc"]);
        assert_eq!(pieces("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // Characters, not bytes.
        assert_eq!(pieces("一二三四五", 2), vec!["一二", "三四", "五"]);
        assert!(pieces("  ", 4).is_empty());
    }
}
