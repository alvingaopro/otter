//! This app's own process, for the performance overlay (D-128): its memory
//! and CPU, as `ps` says. The pages aren't in it — WebKit runs them in
//! processes of its own — so the overlay calls it "app".

use serde::Serialize;

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessStats {
    rss_bytes: u64,
    cpu_percent: f64,
}

#[tauri::command]
pub async fn process_stats() -> Result<ProcessStats, String> {
    let out = tokio::process::Command::new("ps")
        .args(["-o", "rss=,%cpu=", "-p", &std::process::id().to_string()])
        .env("LC_ALL", "C")
        .output()
        .await
        .map_err(|e| format!("couldn't run ps: {e}"))?;
    parse(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| "ps said nothing".to_owned())
}

/// `ps -o rss=,%cpu=`: resident kilobytes, then a percent.
fn parse(out: &str) -> Option<ProcessStats> {
    let mut words = out.split_whitespace();
    let rss: u64 = words.next()?.parse().ok()?;
    let cpu: f64 = words.next()?.parse().ok()?;
    Some(ProcessStats {
        rss_bytes: rss * 1024,
        cpu_percent: cpu,
    })
}

#[cfg(test)]
mod tests {
    use super::ProcessStats;

    #[test]
    fn reads_what_ps_says() {
        assert_eq!(
            super::parse("  84212   3.5\n"),
            Some(ProcessStats {
                rss_bytes: 84212 * 1024,
                cpu_percent: 3.5
            })
        );
        assert_eq!(super::parse(""), None);
    }

    /// The real `ps`, about this test's own process.
    #[test]
    fn asks_ps_about_this_process() {
        let stats = tauri::async_runtime::block_on(super::process_stats()).unwrap();
        assert!(stats.rss_bytes > 1024 * 1024, "{stats:?}");
    }
}
