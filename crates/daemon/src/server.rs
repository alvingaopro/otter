//! Socket server: one task per connection, RPC mode until a streaming method
//! takes the connection over.

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use workd_protocol::wire::{read_json, write_json};
use workd_protocol::{
    ClientMessage, ErrorCode, PROTOCOL_VERSION, Request, RpcError, ServerMessage,
};

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
            Request::EventsSubscribe => return subscribe(daemon, msg.id, r, w).await,
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

async fn subscribe(
    daemon: Arc<Daemon>,
    id: u64,
    mut r: BufReader<OwnedReadHalf>,
    mut w: OwnedWriteHalf,
) -> Result<()> {
    let mut events = daemon.events.subscribe();
    respond(&mut w, id, Ok(serde_json::Value::Null)).await?;
    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(event) => write_json(&mut w, &ServerMessage::Event { event }).await?,
                Err(RecvError::Lagged(n)) => tracing::warn!("event subscriber lagged by {n}"),
                Err(RecvError::Closed) => return Ok(()),
            },
            // Anything the client sends is ignored; EOF means it went away.
            n = r.read(&mut buf) => if n? == 0 { return Ok(()) },
        }
    }
}
