//! Installing `workd` and `workctl` from a GitHub release into `~/.local/bin`
//! on a host (over SSH) or on this machine, so neither needs a Rust toolchain.
//!
//! The script runs in the host's `sh`: it picks the build for the host's OS
//! and CPU, downloads it with curl or wget, and installs both binaries. For a
//! host whose daemon is older, it can also stop the running daemon so the
//! next connection starts the new one — sessions keep running across that
//! (invariant 4).

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::{ClientError, Transport, sh_quote};

/// Where release artifacts are downloaded from.
pub const RELEASES: &str = "https://github.com/alvingaopro/workd/releases/download";

const TIMEOUT: Duration = Duration::from_secs(300);

/// The script that installs release `version` (`X.Y.Z`). With `stop_home`,
/// it then stops the daemon whose state directory that is (`None` inside:
/// the default `~/.workd`).
pub fn script(version: &str, stop_home: Option<Option<&str>>) -> String {
    let mut s = format!(
        r#"set -eu
v={version}
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) t=linux-x86_64 ;;
  Linux-aarch64 | Linux-arm64) t=linux-aarch64 ;;
  Darwin-*) t=macos-universal ;;
  *) echo "workd has no build for $(uname -sm)" >&2; exit 3 ;;
esac
url="{RELEASES}/v$v/workd-$v-$t.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$url" -o "$tmp/workd.tar.gz" || {{ echo "could not download $url" >&2; exit 4; }}
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$tmp/workd.tar.gz" "$url" || {{ echo "could not download $url" >&2; exit 4; }}
else
  echo "curl or wget is needed to download $url" >&2; exit 4
fi
tar -xzf "$tmp/workd.tar.gz" -C "$tmp"
mkdir -p "$HOME/.local/bin"
for b in workd workctl; do
  install -m 755 "$tmp/workd-$v-$t/$b" "$HOME/.local/bin/$b"
done
echo "installed workd and workctl $v in $HOME/.local/bin"
command -v tmux >/dev/null 2>&1 || echo "note: workd needs tmux 3.2 or newer, which is not installed" >&2
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "note: $HOME/.local/bin is not on your PATH" ;; esac
"#,
        version = sh_quote(version),
    );
    if let Some(home) = stop_home {
        let dir = match home {
            None => "\"$HOME/.workd\"".to_owned(),
            Some(h) => match h.strip_prefix("~/") {
                Some(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
                None => sh_quote(h),
            },
        };
        s.push_str(&format!(
            r#"pid_file={dir}/run/workd.pid
if [ -f "$pid_file" ]; then
  pid=$(cat "$pid_file")
  if ps -p "$pid" -o command= 2>/dev/null | grep -q 'workd.* serve' && kill "$pid" 2>/dev/null; then
    echo "stopped the running workd (pid $pid); sessions keep running"
  fi
fi
"#
        ));
    }
    s
}

/// Install release `version` on the transport's host and return what the
/// script printed. `stop_daemon` also stops that host's running daemon (for
/// an upgrade), using the transport's state directory.
pub async fn install(
    transport: &Transport,
    version: &str,
    stop_daemon: bool,
) -> Result<String, ClientError> {
    let script = script(version, stop_daemon.then(|| transport.home()));
    let mut cmd = transport.shell();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| ClientError::Connect(e.to_string()))?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(script.as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| ClientError::Install("timed out".into()))??;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let text = text.trim().to_owned();
    if out.status.success() {
        Ok(text)
    } else {
        Err(ClientError::Install(if text.is_empty() {
            format!("the install script failed ({})", out.status)
        } else {
            text
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_targets_the_release_and_stops_the_right_daemon() {
        let s = script("1.2.3", None);
        assert!(s.contains("v=1.2.3\n"), "{s}");
        assert!(s.contains("/v$v/workd-$v-$t.tar.gz"), "{s}");
        assert!(!s.contains("pid_file"), "{s}");
        assert!(script("1.2.3", Some(None)).contains("pid_file=\"$HOME/.workd\"/run/workd.pid"));
        assert!(
            script("1.2.3", Some(Some("~/.workd-dev")))
                .contains("pid_file=\"$HOME\"/.workd-dev/run/workd.pid")
        );
        assert!(
            script("1.2.3", Some(Some("/srv/w d"))).contains("pid_file='/srv/w d'/run/workd.pid")
        );
    }

    #[tokio::test]
    async fn script_is_valid_sh() {
        for s in [script("1.2.3", None), script("1.2.3", Some(Some("~/x")))] {
            let out = tokio::process::Command::new("/bin/sh")
                .arg("-n")
                .arg("-c")
                .arg(&s)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
