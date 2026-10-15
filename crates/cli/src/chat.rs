//! `otter chat` (D-093): Otter's conversation on a host — about any task
//! there, apart from each task's own.

use anyhow::{Result, bail};
use otter_core::task::{MessageRole, OtterChat};

use crate::config::{Config, HostEntry};
use crate::output;
use crate::target::connect;

/// How long `otter chat <text>` waits for Otter's answer.
const WAIT: std::time::Duration = std::time::Duration::from_secs(180);

fn host(config: &Config, host: Option<String>) -> Result<HostEntry> {
    Ok(match host {
        Some(h) => config.host(&h)?.clone(),
        None => match (&config.default_host, config.hosts.as_slice()) {
            (Some(d), _) => config.host(d)?.clone(),
            (None, [only]) => only.clone(),
            (None, []) => bail!("no hosts registered; add one with `otter host add <name>`"),
            (None, _) => bail!("several hosts; choose one with --host"),
        },
    })
}

fn who(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "you",
        MessageRole::Controller => "Otter",
        MessageRole::Agent => "agent",
        MessageRole::System => "system",
    }
}

/// With text: send it and print Otter's answer. Without: the conversation
/// lately.
pub async fn run(
    config: &Config,
    host_name: Option<String>,
    text: Vec<String>,
    json: bool,
) -> Result<()> {
    let entry = host(config, host_name)?;
    let mut conn = connect(config, &entry).await?;
    let text = text.join(" ");
    if text.trim().is_empty() {
        let chat = conn.otter_get().await.map_err(too_old)?;
        if json {
            return output::print_json(&chat);
        }
        if chat.messages.is_empty() {
            println!(
                "No conversation with Otter on {} yet: `otter chat <message>`.",
                entry.name
            );
        }
        for m in chat.messages.iter().rev().take(20).rev() {
            println!("{}: {}\n", who(m.role), m.text);
        }
        return Ok(());
    }
    let command_id = crate::tasks::command_id();
    let chat = conn.otter_send(&command_id, &text).await.map_err(too_old)?;
    let Some(sent) = chat
        .messages
        .iter()
        .rev()
        .find(|m| m.correlation_id.as_deref() == Some(command_id.as_str()))
        .map(|m| m.id.to_string())
    else {
        bail!("the message wasn't kept");
    };
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let chat: OtterChat = conn.otter_get().await?;
        if let Some(reply) = chat.messages.iter().find(|m| {
            m.role == MessageRole::Controller && m.correlation_id.as_deref() == Some(sent.as_str())
        }) {
            if json {
                return output::print_json(reply);
            }
            println!("{}", reply.text);
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("no answer from Otter yet; `otter chat` shows it when it comes");
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

/// An otterd before Otter's conversation answers `InvalidRequest`.
fn too_old(e: otter_client::ClientError) -> anyhow::Error {
    match &e {
        otter_client::ClientError::Rpc(r)
            if r.code == otter_protocol::ErrorCode::InvalidRequest =>
        {
            anyhow::anyhow!(
                "this host's otterd is too old for Otter's conversation; update it: `otter host install`"
            )
        }
        _ => e.into(),
    }
}
