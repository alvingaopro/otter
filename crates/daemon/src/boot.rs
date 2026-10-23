//! otterd starts again when its machine does (D-208): on the host otterhub
//! runs on, otterd sets that up for itself — a systemd user unit (with
//! lingering, so it starts without a login) on Linux, a LaunchAgent on
//! macOS. Otherwise otterd starts only when a client dials it.
//!
//! Only otterd is stopped with its unit: the sessions' tmux server is started
//! by otterd and outlives it, as it always has (`KillMode=process`,
//! `AbandonProcessGroup`). A second otterd started this way while one runs
//! finds the lock taken and exits at once, successfully: nothing restarts
//! it.
//!
//! `OTTER_BOOT_SERVICE=off` (tests) leaves the machine as it is.

use std::path::{Path, PathBuf};

use crate::env::EnvMap;

/// Set otterd at `exe` with `home` to start with this machine; what is so
/// now, in a line for Settings.
pub async fn ensure(exe: &Path, home: &Path, env: &EnvMap) -> String {
    if std::env::var("OTTER_BOOT_SERVICE").is_ok_and(|v| v == "off") {
        return "Not set to start with this machine (OTTER_BOOT_SERVICE=off).".into();
    }
    let Some(user_home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return "Not set to start with this machine: $HOME isn't set.".into();
    };
    let r = match std::env::consts::OS {
        "linux" => systemd(exe, home, &user_home, env).await,
        "macos" => launchd(exe, home, &user_home),
        os => Err(format!("otterd can't set that up on {os} yet")),
    };
    match r {
        Ok(line) => line,
        Err(why) => format!("Not set to start with this machine: {why}."),
    }
}

/// The unit's (or agent's) name for otterd with `home`: one per home, so two
/// otterds on one machine have one each.
fn name(home: &Path, user_home: &Path) -> String {
    if home == user_home.join(".otter") {
        return "otterd".into();
    }
    let base: String = home
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    match base.trim_matches('-') {
        "" => "otterd-home".into(),
        b => format!("otterd-{b}"),
    }
}

/// A path as systemd and launchd take it: no quoting is attempted.
fn plain(p: &Path) -> Result<String, String> {
    let s = p.to_string_lossy().into_owned();
    if s.chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '%' | '$'))
    {
        return Err(format!(
            "its path `{s}` has characters a unit can't take as they are"
        ));
    }
    Ok(s)
}

/// The systemd user unit for otterd at `exe` with `home`.
fn unit(exe: &str, home: &str) -> String {
    format!(
        "# Written by otterd (D-208): otterd for {home} starts with this machine.\n\
         [Unit]\n\
         Description=Otter's host daemon (otterd) for {home}\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exe} --home {home} serve\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # Only otterd: the sessions' tmux server outlives it.\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

async fn systemd(
    exe: &Path,
    home: &Path,
    user_home: &Path,
    env: &EnvMap,
) -> Result<String, String> {
    let (exe, home_s) = (plain(exe)?, plain(home)?);
    let systemctl =
        crate::env::which("systemctl", env).ok_or("systemd isn't here (no systemctl)")?;
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home.join(".config"))
        .join("systemd/user");
    let name = format!("{}.service", name(home, user_home));
    let path = dir.join(&name);
    let body = unit(&exe, &home_s);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        std::fs::write(&path, &body).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    // The user's manager is found by its runtime directory, which an otterd
    // started over ssh may not have been given.
    let mut env = env.clone();
    if !env.contains_key("XDG_RUNTIME_DIR") {
        // SAFETY: getuid has no preconditions and cannot fail.
        let dir = format!("/run/user/{}", unsafe { libc::getuid() });
        if Path::new(&dir).is_dir() {
            env.insert("XDG_RUNTIME_DIR".into(), dir);
        }
    }
    let env = &env;
    let run = |args: &'static [&'static str], extra: Option<&str>| {
        let mut cmd = tokio::process::Command::new(&systemctl);
        cmd.args(args).envs(env).kill_on_drop(true);
        if let Some(e) = extra {
            cmd.arg(e);
        }
        async move {
            let out = cmd.output().await.map_err(|e| e.to_string())?;
            match out.status.success() {
                true => Ok(()),
                false => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            }
        }
    };
    run(&["--user", "daemon-reload"], None)
        .await
        .map_err(|e| format!("systemd didn't take its unit ({e})"))?;
    // Enabled, not started: this otterd is running already.
    run(&["--user", "enable"], Some(&name))
        .await
        .map_err(|e| format!("systemd didn't enable {name} ({e})"))?;
    let user = std::env::var("USER").unwrap_or_default();
    Ok(match linger(&user, env).await {
        Ok(()) => format!("Starts with this machine: systemd user unit {name}."),
        Err(why) => format!(
            "Starts when you log in, not with the machine: systemd user unit {name}, but lingering is off ({why}); `sudo loginctl enable-linger {user}` turns it on."
        ),
    })
}

/// The user's units start without a login (`loginctl enable-linger`).
async fn linger(user: &str, env: &EnvMap) -> Result<(), String> {
    let loginctl = crate::env::which("loginctl", env).ok_or("no loginctl")?;
    let on = tokio::process::Command::new(&loginctl)
        .args(["show-user", user, "-p", "Linger", "--value"])
        .envs(env)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if String::from_utf8_lossy(&on.stdout).trim() == "yes" {
        return Ok(());
    }
    let set = tokio::process::Command::new(&loginctl)
        .args(["enable-linger", user])
        .envs(env)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    match set.status.success() {
        true => Ok(()),
        false => Err(String::from_utf8_lossy(&set.stderr).trim().to_owned()),
    }
}

/// The LaunchAgent for otterd at `exe` with `home`, labelled `label`.
fn agent(label: &str, exe: &str, home: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by otterd (D-208): otterd for {home} starts at login. -->\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key><string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>{exe}</string>\n\t\t<string>--home</string>\n\t\t<string>{home}</string>\n\t\t<string>serve</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key><true/>\n\
         \t<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
         \t<!-- Only otterd: the sessions' tmux server outlives it. -->\n\
         \t<key>AbandonProcessGroup</key><true/>\n\
         </dict>\n</plist>\n"
    )
}

fn launchd(exe: &Path, home: &Path, user_home: &Path) -> Result<String, String> {
    let (exe, home_s) = (plain(exe)?, plain(home)?);
    let label = format!("io.otter.{}", name(home, user_home));
    let dir = user_home.join("Library/LaunchAgents");
    let path = dir.join(format!("{label}.plist"));
    let body = agent(&label, &exe, &home_s);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        std::fs::write(&path, &body).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    // Loaded at the next login: loading it now would start a second otterd.
    Ok(format!(
        "Starts when you log in: LaunchAgent {label} (from the next login)."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One unit per home: the default one is `otterd`, another is named
    /// after its directory.
    #[test]
    fn each_home_has_a_unit_of_its_own() {
        let user = Path::new("/home/u");
        assert_eq!(name(Path::new("/home/u/.otter"), user), "otterd");
        assert_eq!(name(Path::new("/home/u/.workd"), user), "otterd-workd");
        assert_eq!(
            name(Path::new("/srv/otter home"), user),
            "otterd-otter-home"
        );
        assert_eq!(name(Path::new("/"), user), "otterd-home");
    }

    /// The unit starts otterd with its home, restarts it only on a failure
    /// (a second one exits 0 when one runs), and stops only otterd.
    #[test]
    fn the_unit_stops_only_otterd() {
        let u = unit("/home/u/.local/bin/otterd", "/home/u/.workd");
        assert!(u.contains("ExecStart=/home/u/.local/bin/otterd --home /home/u/.workd serve\n"));
        assert!(u.contains("Restart=on-failure\n"));
        assert!(u.contains("KillMode=process\n"));
        assert!(u.contains("WantedBy=default.target\n"));
        let a = agent(
            "io.otter.otterd",
            "/Users/u/.local/bin/otterd",
            "/Users/u/.otter",
        );
        assert!(a.contains("<string>/Users/u/.local/bin/otterd</string>"));
        assert!(a.contains("<key>AbandonProcessGroup</key><true/>"));
        assert!(a.contains("<key>SuccessfulExit</key><false/>"));
    }

    /// A path a unit would need quoted is refused, not half-quoted.
    #[test]
    fn a_path_a_unit_cant_take_is_refused() {
        assert!(plain(Path::new("/home/u/.otter")).is_ok());
        for bad in [
            "/home/u/my otter",
            "/home/u/$x",
            "/home/u/a%b",
            "/home/u/\"q\"",
        ] {
            assert!(plain(Path::new(bad)).is_err(), "{bad}");
        }
    }
}
