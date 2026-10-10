//! Installing `otterd` and `otter` from a GitHub release into `~/.local/bin`
//! on a host (over SSH) or on this machine, so neither needs a Rust toolchain.
//!
//! The script runs in the host's `sh`: it picks the build for the host's OS
//! and CPU, downloads it with curl or wget, and installs both binaries.
//!
//! Then the Claude worker (D-060) — Node code plus the Claude Code its SDK
//! bundles for this platform — into `~/.local/lib/otter/claude-runtime/<version>`,
//! where that release's `otterd` looks for it. It is staged, checked against
//! its published SHA-256, asked `--check` when Node is there, and only then
//! moved into place; a directory per release, so what a running worker uses
//! is never replaced. The release before stays (older ones go). A worker that
//! can't be installed is a note, not a failure: otterd works without it. For a
//! host whose daemon is older, it can also stop the running daemon so the
//! next connection starts the new one — sessions keep running across that
//! (invariant 4).

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::{ClientError, Transport, sh_quote};

/// Where release artifacts are downloaded from.
pub const RELEASES: &str = "https://github.com/alvingaopro/otter/releases/download";

const TIMEOUT: Duration = Duration::from_secs(300);

/// The script that installs release `version` (`X.Y.Z`). With `stop_home`,
/// it then stops the daemon whose state directory that is (`None` inside:
/// the default `~/.otter`).
pub fn script(version: &str, stop_home: Option<Option<&str>>) -> String {
    let mut s = format!(
        r#"set -eu
v={version}
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) t=linux-x86_64 ;;
  Linux-aarch64 | Linux-arm64) t=linux-aarch64 ;;
  Darwin-arm64) t=macos-arm64 ;;
  Darwin-x86_64) t=macos-x86_64 ;;
  *) echo "otterd has no build for $(uname -sm)" >&2; exit 3 ;;
esac
base="{RELEASES}/v$v"
url="$base/otter-$v-$t.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fetch() {{
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"
  else echo "curl or wget is needed to download $1" >&2; exit 4
  fi
}}
fetch "$url" "$tmp/otterd.tar.gz" || {{ echo "could not download $url" >&2; exit 4; }}
tar -xzf "$tmp/otterd.tar.gz" -C "$tmp"
mkdir -p "$HOME/.local/bin"
for b in otterd otter; do
  install -m 755 "$tmp/otter-$v-$t/$b" "$HOME/.local/bin/$b"
done
echo "installed otterd and otter $v in $HOME/.local/bin"
command -v tmux >/dev/null 2>&1 || echo "note: otterd needs tmux 3.2 or newer, which is not installed" >&2
rt="$HOME/.local/lib/otter/claude-runtime"
w="otter-claude-runtime-$v-$t"
if [ -f "$rt/$v/dist/main.js" ]; then
  echo "the Claude worker $v is already installed"
elif ! fetch "$base/$w.tar.gz" "$tmp/w.tar.gz" 2>/dev/null || ! fetch "$base/$w.tar.gz.sha256" "$tmp/w.sha256" 2>/dev/null; then
  echo "note: release $v has no Claude worker for $t; Claude's SDK mode won't be available" >&2
else
  want=$(cut -d' ' -f1 "$tmp/w.sha256")
  got=$( (sha256sum "$tmp/w.tar.gz" 2>/dev/null || shasum -a 256 "$tmp/w.tar.gz") | cut -d' ' -f1)
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    echo "note: the Claude worker download doesn't match its checksum; not installed" >&2
  else
    mkdir -p "$rt"
    stage="$rt/.staging-$v-$$"
    rm -rf "$stage" && mkdir "$stage"
    tar -xzf "$tmp/w.tar.gz" -C "$stage"
    ok=1
    if command -v node >/dev/null 2>&1; then
      node "$stage/$w/dist/main.js" --check >"$tmp/check" 2>/dev/null || ok=0
      [ "$ok" = 1 ] || echo "note: the Claude worker can't run here: $(cat "$tmp/check") (it needs Node.js 20 or newer)" >&2
    else
      echo "note: the Claude worker needs Node.js 20 or newer, which isn't on PATH here" >&2
    fi
    if [ "$ok" = 1 ]; then
      mv "$stage/$w" "$rt/$v"
      echo "installed the Claude worker $v in $rt/$v"
      # Keep this release's and the one before; older ones go.
      ls -1t "$rt" | tail -n +3 | while IFS= read -r old; do rm -rf "${{rt:?}}/$old"; done
    fi
    rm -rf "$stage"
  fi
fi
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "note: $HOME/.local/bin is not on your PATH" ;; esac
"#,
        version = sh_quote(version),
    );
    if let Some(home) = stop_home {
        let dir = match home {
            // Default home, or the pre-rename one (D-024).
            None => "\"$HOME/.otter\" \"$HOME/.workd\"".to_owned(),
            Some(h) => match h.strip_prefix("~/") {
                Some(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
                None => sh_quote(h),
            },
        };
        s.push_str(&format!(
            r#"for home in {dir}; do
  pid_file="$home/run/workd.pid"
  [ -f "$pid_file" ] || continue
  pid=$(cat "$pid_file")
  if ps -p "$pid" -o command= 2>/dev/null | grep -Eq '(otterd|workd).* serve' && kill "$pid" 2>/dev/null; then
    echo "stopped the running daemon (pid $pid); sessions keep running"
  fi
done
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
        assert!(s.contains("url=\"$base/otter-$v-$t.tar.gz\""), "{s}");
        // The worker: checked, then moved into a directory of its own release.
        assert!(s.contains("$base/$w.tar.gz.sha256"), "{s}");
        assert!(s.contains("mv \"$stage/$w\" \"$rt/$v\""), "{s}");
        assert!(!s.contains("pid_file"), "{s}");
        assert!(
            script("1.2.3", Some(None))
                .contains("for home in \"$HOME/.otter\" \"$HOME/.workd\"; do")
        );
        assert!(
            script("1.2.3", Some(Some("~/.otter-dev")))
                .contains("for home in \"$HOME\"/.otter-dev; do")
        );
        assert!(script("1.2.3", Some(Some("/srv/w d"))).contains("for home in '/srv/w d'; do"));
    }

    /// The worker part, against a release served from a directory: installed
    /// when its checksum matches, refused when it doesn't, never fatal.
    #[tokio::test]
    async fn the_worker_is_verified_then_moved_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let rel = dir.path().join("rel/v9.9.9");
        let w = "otter-claude-runtime-9.9.9-test";
        let pkg = dir.path().join("pkg").join(w).join("dist");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(&rel).unwrap();
        std::fs::write(pkg.join("main.js"), "// worker").unwrap();
        let tar = rel.join(format!("{w}.tar.gz"));
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tar)
            .arg("-C")
            .arg(dir.path().join("pkg"))
            .arg(w)
            .status()
            .unwrap();
        assert!(ok.success());
        let sum =
            |text: &str| std::fs::write(rel.join(format!("{w}.tar.gz.sha256")), text).unwrap();
        let hash = {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg("(sha256sum \"$0\" 2>/dev/null || shasum -a 256 \"$0\") | cut -d' ' -f1")
                .arg(&tar)
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        // Only the worker part of the script, with `fetch` reading files and
        // no Node (so no `--check`).
        let full = script("9.9.9", None);
        let part = &full[full.find("rt=").unwrap()..full.find("case \":$PATH:\"").unwrap()];
        let run = |home: &std::path::Path| {
            let prelude = format!(
                "set -eu\nv=9.9.9\nt=test\nbase={}\ntmp=$(mktemp -d)\nfetch() {{ cp \"$1\" \"$2\"; }}\nPATH=/usr/bin:/bin\nHOME={}\n",
                rel.display(),
                home.display()
            );
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("{prelude}{part}"))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        };
        let installed = |home: &std::path::Path| {
            home.join(".local/lib/otter/claude-runtime/9.9.9/dist/main.js")
                .is_file()
        };

        let home = dir.path().join("bad");
        sum("0000  x\n");
        let out = run(&home);
        assert!(out.contains("doesn't match its checksum"), "{out}");
        assert!(!installed(&home));

        let home = dir.path().join("good");
        sum(&format!("{hash}  {w}.tar.gz\n"));
        let out = run(&home);
        assert!(out.contains("installed the Claude worker 9.9.9"), "{out}");
        assert!(installed(&home));
        let again = run(&home);
        assert!(again.contains("already installed"), "{again}");
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
