//! Browser login for terminal clients (D-032): while [`Logins`] runs, this
//! machine is the browser for a host's sign-in pages, as the desktop app is.
//!
//! It subscribes with `browser: true`, takes each page announced by
//! `BrowserOpenRequested` (`browser.take`, once — with the app also connected,
//! whichever takes it first opens it, never both), maps a loopback callback
//! port to this machine over the shared SSH connection, and opens the page.
//! Callback mappings go when the host stops listening on the port (the tool
//! got its redirect), after ten minutes, or when [`Logins::stop`] runs.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otter_protocol::Event;
use tokio::task::{JoinHandle, JoinSet};

use crate::forward::{self, Direction, Mapping};
use crate::{Connection, EventStream, Result, Transport};

/// Opens a URL on this machine.
pub type Opener = Arc<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;

/// How long a callback port stays mapped at most (as in the app).
const LOGIN_TTL: Duration = Duration::from_secs(600);
/// How often finished logins are looked for.
const REAP_EVERY: Duration = Duration::from_secs(5);

/// Callback mappings in place, with when they expire.
type Callbacks = Arc<Mutex<Vec<(Mapping, Instant)>>>;

pub struct Logins {
    transport: Transport,
    task: JoinHandle<()>,
    callbacks: Callbacks,
}

impl Logins {
    /// Subscribe as the browser for `host` and serve its sign-in pages in the
    /// background. Returns once subscribed, so a login started right after is
    /// caught.
    pub async fn start(transport: Transport, host: &str, open: Opener) -> Result<Logins> {
        let stream = Connection::connect(&transport)
            .await?
            .subscribe_for_browser(None)
            .await?;
        let callbacks = Callbacks::default();
        let task = tokio::spawn(serve(
            transport.clone(),
            host.to_owned(),
            open,
            callbacks.clone(),
            stream,
        ));
        Ok(Logins {
            transport,
            task,
            callbacks,
        })
    }

    /// Stop serving and remove the callback mappings still in place.
    pub async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
        let left = std::mem::take(&mut *self.callbacks.lock().unwrap());
        for (mapping, _) in left {
            let _ = forward::cancel(&self.transport, &mapping).await;
        }
    }
}

async fn serve(
    transport: Transport,
    host: String,
    open: Opener,
    callbacks: Callbacks,
    mut stream: EventStream,
) {
    // Dropped (aborting in-flight logins) when this task is.
    let mut logins = JoinSet::new();
    let mut reap = tokio::time::interval(REAP_EVERY);
    loop {
        // Event reads aren't cancel-safe: reap between requests, not during.
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let reader = tokio::spawn(async move {
            while let Ok(Some(rec)) = stream.next().await {
                if let Event::BrowserOpenRequested { request_id, .. } = rec.event
                    && tx.send(request_id).await.is_err()
                {
                    return;
                }
            }
        });
        let _reader = AbortOnDrop(reader);
        loop {
            tokio::select! {
                id = rx.recv() => match id {
                    Some(id) => {
                        logins.spawn(open_login(
                            transport.clone(),
                            host.clone(),
                            id,
                            open.clone(),
                            callbacks.clone(),
                        ));
                    }
                    None => break,
                },
                _ = reap.tick() => reap_logins(&transport, &callbacks).await,
                Some(_) = logins.join_next() => {}
            }
        }
        // The connection dropped: subscribe again.
        stream = loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(conn) = Connection::connect(&transport).await
                && let Ok(stream) = conn.subscribe_for_browser(None).await
            {
                break stream;
            }
        };
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Take one sign-in page, map its callback port here, and open it.
async fn open_login(
    transport: Transport,
    host: String,
    request_id: String,
    open: Opener,
    callbacks: Callbacks,
) {
    let Ok(mut conn) = Connection::connect(&transport).await else {
        return;
    };
    // Taken by another client (the app, another terminal) first: theirs.
    let Ok(opening) = conn.browser_take(&request_id).await else {
        return;
    };
    // A local host's callback port is already on this machine.
    if let (Some(port), Transport::Ssh { .. }) = (opening.callback_port, &transport) {
        let mapping = Mapping {
            host,
            direction: Direction::ToLocal,
            listen_port: port,
            target_host: "localhost".into(),
            target_port: port,
            pinned: false,
        };
        // Recorded first, so a stop during `apply` still removes it.
        callbacks
            .lock()
            .unwrap()
            .push((mapping.clone(), Instant::now() + LOGIN_TTL));
        // On failure the page still opens; the tool can take a pasted code.
        let _ = forward::apply(&transport, &mapping).await;
    }
    let url = opening.url;
    let _ = tokio::task::spawn_blocking(move || open(&url)).await;
}

/// Drop callbacks whose time is up or whose port the host no longer listens
/// on (the tool got its redirect and exited).
async fn reap_logins(transport: &Transport, callbacks: &Callbacks) {
    let current: Vec<(Mapping, Instant)> = callbacks.lock().unwrap().clone();
    if current.is_empty() {
        return;
    }
    let listening = match Connection::connect(transport).await {
        Ok(mut conn) => conn.host_ports().await.ok(),
        Err(_) => None,
    };
    for (mapping, until) in current {
        let done = Instant::now() > until
            || listening
                .as_ref()
                .is_some_and(|ports| !ports.iter().any(|p| p.port == mapping.target_port));
        if done {
            let _ = forward::cancel(transport, &mapping).await;
            callbacks
                .lock()
                .unwrap()
                .retain(|(m, _)| !m.same_slot(&mapping));
        }
    }
}

/// Open `url` in this machine's browser: `open` on macOS, `xdg-open`
/// elsewhere.
pub fn system_open(url: &str) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{program} exited with {status}"
        )))
    }
}
