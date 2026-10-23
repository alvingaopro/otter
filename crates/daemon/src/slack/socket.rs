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
    /// Anything else Slack wants acked (a slash command, a button): not taken
    /// yet.
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
        (_, Some(envelope_id)) => Envelope::Other { envelope_id },
        _ => Envelope::Unknown,
    }
}

/// A direct message from the developer, as Otter takes it.
#[derive(Debug, PartialEq)]
pub struct Inbound {
    pub to: Origin,
    pub ts: String,
    pub text: String,
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
    if text.trim().is_empty() {
        return Err(Skip::Empty);
    }
    Ok(Inbound {
        to: Origin {
            channel: channel.to_owned(),
            thread: event["thread_ts"].as_str().map(String::from),
        },
        ts: ts.to_owned(),
        text: text.trim().to_owned(),
    })
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
            envelope(r#"{"type":"slash_commands","envelope_id":"e2","payload":{}}"#),
            Envelope::Other {
                envelope_id: "e2".into()
            }
        );
        assert_eq!(envelope("not json"), Envelope::Unknown);
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
