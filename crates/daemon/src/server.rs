//! Socket server: one task per connection, RPC mode until a streaming method
//! takes the connection over.

use std::sync::Arc;

use anyhow::Result;
use otter_protocol::wire::{read_json, write_json};
use otter_protocol::{
    ClientMessage, ErrorCode, EventRecord, EventsSubscribe, PROTOCOL_VERSION, Request, RpcError,
    ServerMessage, Subscribed,
};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;

use crate::attach;
use crate::daemon::Daemon;

pub async fn serve(daemon: Arc<Daemon>, listener: UnixListener) -> Result<()> {
    let mut shutdown = daemon.shutdown_signal();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(daemon, stream).await {
                        tracing::debug!("connection ended with error: {e:#}");
                    }
                });
            }
            _ = shutdown.wait_for(|stop| *stop) => return Ok(()),
        }
    }
}

async fn handle_connection(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    write_json(
        &mut w,
        &ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    )
    .await?;

    loop {
        let Some(raw) = read_json::<_, serde_json::Value>(&mut r).await? else {
            return Ok(());
        };
        let msg: ClientMessage = match serde_json::from_value(raw.clone()) {
            Ok(m) => m,
            Err(e) => {
                let id = raw.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                respond(
                    &mut w,
                    id,
                    Err(RpcError::new(ErrorCode::InvalidRequest, e.to_string())),
                )
                .await?;
                continue;
            }
        };
        tracing::debug!(method = msg.request.method(), id = msg.id, "request");
        match msg.request {
            Request::SessionAttach(params) => {
                return attach::run(daemon, msg.id, params, r, w).await;
            }
            Request::EventsSubscribe(params) => {
                return subscribe(daemon, msg.id, params, r, w).await;
            }
            Request::Shutdown => {
                respond(&mut w, msg.id, Ok(serde_json::Value::Null)).await?;
                tracing::info!("shutdown requested");
                daemon.request_shutdown();
                return Ok(());
            }
            req => {
                let result = daemon.handle(req).await;
                respond(&mut w, msg.id, result).await?;
            }
        }
    }
}

pub async fn respond(
    w: &mut OwnedWriteHalf,
    id: u64,
    result: Result<serde_json::Value, RpcError>,
) -> std::io::Result<()> {
    let msg = match result {
        Ok(v) => ServerMessage::Response {
            id,
            result: Some(v),
            error: None,
        },
        Err(e) => ServerMessage::Response {
            id,
            result: None,
            error: Some(e),
        },
    };
    write_json(w, &msg).await
}

/// Stream events after `params.after` (replayed from the log), then live ones,
/// each exactly once and in `seq` order (`docs/protocol.md`).
async fn subscribe(
    daemon: Arc<Daemon>,
    id: u64,
    params: EventsSubscribe,
    mut r: BufReader<OwnedReadHalf>,
    mut w: OwnedWriteHalf,
) -> Result<()> {
    let (head, mut live) = daemon.events.subscribe();
    let _browser = params
        .browser
        .then(|| crate::daemon::BrowserSubscriber::new(&daemon));
    let log_id = daemon.events.log_id();
    let backlog = match params.after {
        None => Vec::new(),
        // A cursor from another log (this one started over) can't be
        // compared by `seq` at all.
        Some(after) if params.log_id.as_deref().is_some_and(|l| l != log_id) => {
            let message = format!(
                "event cursor {after} is from an earlier event log; the host's log started over"
            );
            let err = RpcError::new(ErrorCode::CursorExpired, message);
            return Ok(respond(&mut w, id, Err(err)).await?);
        }
        Some(after) => match daemon.events.since(after) {
            Ok(Some(records)) => records,
            Ok(None) => {
                let message = format!("event cursor {after} is not available (latest is {head})");
                let err = RpcError::new(ErrorCode::CursorExpired, message);
                return Ok(respond(&mut w, id, Err(err)).await?);
            }
            Err(e) => {
                let err = RpcError::internal(format!("{e:#}"));
                return Ok(respond(&mut w, id, Err(err)).await?);
            }
        },
    };
    let ready = serde_json::to_value(Subscribed {
        seq: head,
        log_id: Some(log_id.to_owned()),
    })?;
    respond(&mut w, id, Ok(ready)).await?;

    let mut sent = params.after.unwrap_or(head);
    send_after(&mut w, &mut sent, backlog).await?;
    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            ev = live.recv() => match ev {
                Ok(event) => send_after(&mut w, &mut sent, [event]).await?,
                // Fell behind the broadcast buffer: catch up from the log
                // rather than skip events. Overlap with what is still
                // buffered is filtered by `seq`.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("event subscriber lagged by {n}; replaying from the log");
                    let Some(missed) = daemon.events.since(sent)? else {
                        // Rotated away meanwhile: end the stream rather than
                        // skip events; the client's cursor is refused on
                        // reconnect and it reloads a snapshot.
                        tracing::warn!("event subscriber fell behind the retained log");
                        return Ok(());
                    };
                    send_after(&mut w, &mut sent, missed).await?;
                }
                Err(RecvError::Closed) => return Ok(()),
            },
            // Anything the client sends is ignored; EOF means it went away.
            n = r.read(&mut buf) => if n? == 0 { return Ok(()) },
        }
    }
}

/// Send the records newer than `sent`, advancing it.
async fn send_after(
    w: &mut OwnedWriteHalf,
    sent: &mut u64,
    records: impl IntoIterator<Item = EventRecord>,
) -> Result<()> {
    for event in records {
        if event.seq > *sent {
            *sent = event.seq;
            write_json(w, &ServerMessage::Event { event }).await?;
        }
    }
    Ok(())
}
