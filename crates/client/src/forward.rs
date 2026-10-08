//! Port mappings between this machine and a host, carried by the host's
//! shared SSH connection (the ControlMaster `otterd dial` already uses).
//!
//! - **To here** (`-L`): a port on the host (or a machine it reaches) appears
//!   on this machine's `127.0.0.1` — open a remote dev server locally.
//! - **To the host** (`-R`): a port here appears on the host's `127.0.0.1`.
//!
//! Mappings are added and removed on the running connection (`ssh -O forward`
//! / `-O cancel`), and die with it; callers re-apply them when the
//! connection's master process changes ([`master_pid`]). Pinned mappings are
//! kept in `ports.toml` next to `hosts.toml`.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{ClientError, Transport};

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// `-L`: appears on this machine.
    ToLocal,
    /// `-R`: appears on the host.
    ToHost,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Mapping {
    pub host: String,
    pub direction: Direction,
    /// The port that appears (on this machine for `to_local`, on the host
    /// for `to_host`), bound to 127.0.0.1.
    pub listen_port: u16,
    /// Where connections go, resolved on the other side: on the host for
    /// `to_local`, here for `to_host`.
    #[serde(default = "localhost")]
    pub target_host: String,
    pub target_port: u16,
    /// Kept across restarts.
    #[serde(default)]
    pub pinned: bool,
}

fn localhost() -> String {
    "localhost".to_owned()
}

impl Mapping {
    /// `ssh` flag and spec: `-L 127.0.0.1:5173:localhost:5173`.
    fn spec(&self) -> [String; 2] {
        let flag = match self.direction {
            Direction::ToLocal => "-L",
            Direction::ToHost => "-R",
        };
        let target = if self.target_host.contains(':') {
            format!("[{}]", self.target_host)
        } else {
            self.target_host.clone()
        };
        [
            flag.to_owned(),
            format!(
                "127.0.0.1:{}:{target}:{}",
                self.listen_port, self.target_port
            ),
        ]
    }

    /// Identity on a host: one mapping per direction and listening port.
    pub fn same_slot(&self, other: &Mapping) -> bool {
        self.host == other.host
            && self.direction == other.direction
            && self.listen_port == other.listen_port
    }
}

async fn control(transport: &Transport, extra: Vec<String>) -> Result<String, ClientError> {
    let Transport::Ssh { control_dir, .. } = transport else {
        return Err(ClientError::Forward(
            "port mappings are for hosts reached over SSH".into(),
        ));
    };
    if control_dir.is_none() {
        return Err(ClientError::Forward(
            "SSH connection sharing is unavailable (config directory path too long)".into(),
        ));
    }
    let mut cmd = transport.ssh_with(&extra).expect("ssh transport");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .map_err(|_| ClientError::Forward("ssh timed out".into()))?
        .map_err(|e| ClientError::Forward(e.to_string()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .trim()
    .to_owned();
    if out.status.success() {
        Ok(text)
    } else {
        Err(ClientError::Forward(if text.is_empty() {
            format!("ssh exited with {}", out.status)
        } else {
            text
        }))
    }
}

/// The pid of the host's shared connection, if it is up.
pub async fn master_pid(transport: &Transport) -> Option<u32> {
    let out = control(transport, vec!["-O".into(), "check".into()])
        .await
        .ok()?;
    // "Master running (pid=12345)"
    out.split("pid=")
        .nth(1)?
        .split(')')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Make sure the shared connection is up (starting it if needed); its pid.
pub async fn ensure_master(transport: &Transport) -> Result<u32, ClientError> {
    if !matches!(transport, Transport::Ssh { .. }) {
        return Err(ClientError::Forward(
            "port mappings are for hosts reached over SSH".into(),
        ));
    }
    if let Some(pid) = master_pid(transport).await {
        return Ok(pid);
    }
    // Any command through the transport starts a persistent master.
    let mut cmd = transport
        .ssh_with(&[])
        .ok_or_else(|| ClientError::Forward("not an SSH host".into()))?;
    cmd.arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .map_err(|_| ClientError::Forward("ssh timed out".into()))?
        .map_err(|e| ClientError::Forward(e.to_string()))?;
    if !out.status.success() {
        return Err(ClientError::Forward(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    master_pid(transport)
        .await
        .ok_or_else(|| ClientError::Forward("the SSH connection did not stay up".into()))
}

/// Add a mapping to the host's shared connection.
pub async fn apply(transport: &Transport, mapping: &Mapping) -> Result<(), ClientError> {
    ensure_master(transport).await?;
    let mut args = vec!["-O".into(), "forward".into()];
    args.extend(mapping.spec());
    control(transport, args)
        .await
        .map(|_| ())
        .map_err(|e| match e {
            ClientError::Forward(msg) if msg.contains("forwarding failed") => {
                ClientError::Forward(format!(
                    "port {} is already in use {}",
                    mapping.listen_port,
                    match mapping.direction {
                        Direction::ToLocal => "on this machine",
                        Direction::ToHost => "on the host",
                    }
                ))
            }
            other => other,
        })
}

/// Remove a mapping from the host's shared connection.
pub async fn cancel(transport: &Transport, mapping: &Mapping) -> Result<(), ClientError> {
    let mut args = vec!["-O".into(), "cancel".into()];
    args.extend(mapping.spec());
    control(transport, args).await.map(|_| ())
}

// ---------------------------------------------------------------------------
// Pinned mappings (`ports.toml`)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct Pins {
    #[serde(default, rename = "mapping")]
    mappings: Vec<Mapping>,
}

/// Pinned mappings, in `dir/ports.toml`.
pub fn load_pins(dir: &Path) -> anyhow::Result<Vec<Mapping>> {
    let path = dir.join("ports.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(toml::from_str::<Pins>(&text)?.mappings),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_pins(dir: &Path, mappings: &[Mapping]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let pins = Pins {
        mappings: mappings.iter().filter(|m| m.pinned).cloned().collect(),
    };
    let tmp = dir.join("ports.toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(&pins)?)?;
    std::fs::rename(&tmp, dir.join("ports.toml"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(direction: Direction) -> Mapping {
        Mapping {
            host: "dev".into(),
            direction,
            listen_port: 15432,
            target_host: "db.internal".into(),
            target_port: 5432,
            pinned: true,
        }
    }

    #[test]
    fn specs_bind_loopback() {
        assert_eq!(
            m(Direction::ToLocal).spec(),
            ["-L", "127.0.0.1:15432:db.internal:5432"]
        );
        assert_eq!(
            m(Direction::ToHost).spec(),
            ["-R", "127.0.0.1:15432:db.internal:5432"]
        );
        let mut v6 = m(Direction::ToLocal);
        v6.target_host = "::1".into();
        assert_eq!(v6.spec()[1], "127.0.0.1:15432:[::1]:5432");
    }

    #[test]
    fn pins_round_trip_and_drop_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let mut temp = m(Direction::ToHost);
        temp.pinned = false;
        temp.listen_port = 9000;
        save_pins(dir.path(), &[m(Direction::ToLocal), temp]).unwrap();
        assert_eq!(load_pins(dir.path()).unwrap(), vec![m(Direction::ToLocal)]);
        let text = std::fs::read_to_string(dir.path().join("ports.toml")).unwrap();
        assert!(text.contains("direction = \"to_local\""), "{text}");
    }

    #[test]
    fn local_hosts_have_no_mappings() {
        let local = Transport::Local {
            otterd_path: "otterd".into(),
            home: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(apply(&local, &m(Direction::ToLocal)))
            .unwrap_err();
        assert!(err.to_string().contains("over SSH"), "{err}");
    }
}
