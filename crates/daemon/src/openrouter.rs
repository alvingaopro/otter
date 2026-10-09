//! OpenRouter as the Control Agent's model (D-045): one structured answer
//! per call, through the chat completions API.
//!
//! The key is `OPENROUTER_API_KEY` in otterd's environment (the login
//! environment, e.g. exported in the shell profile). It never goes on a
//! command line, into an event, or into a feature: requests go through
//! `curl` with the key in its config on stdin and the body in a private
//! temporary file. `OTTER_CONTROLLER_MODEL` picks the model (default
//! `openrouter/auto`).

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::env::{EnvMap, which};

const URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEFAULT_MODEL: &str = "openrouter/auto";

/// The key, from otterd's environment.
pub fn key(env: &EnvMap) -> Option<String> {
    env.get("OPENROUTER_API_KEY")
        .cloned()
        .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty())
}

pub fn model() -> String {
    std::env::var("OTTER_CONTROLLER_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.into())
}

/// The request: the prompt (plus an image, for a visual review), asking for
/// JSON that fits `schema`.
pub fn request_body(model: &str, prompt: &str, schema: &Value, png: Option<&[u8]>) -> Value {
    let text = format!(
        "{prompt}\n\nAnswer with one JSON object only, matching this JSON schema:\n{schema}"
    );
    let content = match png {
        Some(bytes) => json!([
            {"type": "text", "text": text},
            {"type": "image_url", "image_url": {"url": format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )}}
        ]),
        None => json!(text),
    };
    json!({
        "model": model,
        "messages": [{"role": "user", "content": content}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "answer", "strict": true, "schema": schema}
        }
    })
}

/// The JSON answer in a chat completion (models that ignore the response
/// format may wrap it in prose or a code fence).
pub fn parse(response: &Value) -> Result<Value> {
    if let Some(e) = response.get("error") {
        bail!(
            "OpenRouter: {}",
            e["message"].as_str().unwrap_or("request failed")
        );
    }
    let text = response["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow!("OpenRouter gave no answer"))?;
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return Ok(v);
    }
    let (start, end) = (text.find('{'), text.rfind('}'));
    match (start, end) {
        (Some(s), Some(e)) if e > s => {
            serde_json::from_str(&text[s..=e]).context("reading OpenRouter's answer")
        }
        _ => bail!("OpenRouter's answer isn't JSON"),
    }
}

/// One structured answer. `image`: a PNG to look at.
pub async fn structured(
    env: &EnvMap,
    prompt: &str,
    schema: &Value,
    image: Option<&Path>,
) -> Result<Value> {
    let key = key(env).ok_or_else(|| anyhow!("OPENROUTER_API_KEY is not set for otterd"))?;
    let curl = which("curl", env).ok_or_else(|| anyhow!("curl is not installed"))?;
    let png = match image {
        Some(p) => Some(std::fs::read(p).with_context(|| format!("reading {}", p.display()))?),
        None => None,
    };
    let body = request_body(&model(), prompt, schema, png.as_deref());
    // The body in a private file; the key only in curl's config, on stdin.
    let mut file = tempfile::Builder::new().prefix("otter-or").tempfile()?;
    file.write_all(body.to_string().as_bytes())?;
    let config = format!(
        "header = \"Authorization: Bearer {key}\"\nheader = \"Content-Type: application/json\"\nheader = \"X-Title: Otter\"\n"
    );
    let mut child = tokio::process::Command::new(&curl)
        .args(["-sS", "--max-time", "300", "--config", "-", "--data-binary"])
        .arg(format!("@{}", file.path().display()))
        .arg(URL)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting curl")?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(config.as_bytes()).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "reaching OpenRouter: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("reading OpenRouter's response")?;
    parse(&v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn requests_ask_for_schema_json_and_carry_images() {
        let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let b = request_body("m/x", "Judge it.", &schema, None);
        assert_eq!(b["model"], "m/x");
        assert_eq!(b["response_format"]["json_schema"]["schema"], schema);
        assert!(
            b["messages"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("Judge it.")
        );
        let b = request_body("m/x", "Look.", &schema, Some(b"\x89PNG"));
        assert_eq!(b["messages"][0]["content"][1]["type"], "image_url");
        assert!(
            b["messages"][0]["content"][1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }

    #[test]
    fn answers_are_read_even_when_wrapped() {
        let r = |text: &str| json!({"choices": [{"message": {"content": text}}]});
        assert_eq!(parse(&r(r#"{"ok": true}"#)).unwrap(), json!({"ok": true}));
        assert_eq!(
            parse(&r("Here:\n```json\n{\"ok\": false}\n```")).unwrap(),
            json!({"ok": false})
        );
        assert!(parse(&r("no idea")).is_err());
        let err = parse(&json!({"error": {"message": "No auth credentials found"}})).unwrap_err();
        assert!(format!("{err}").contains("No auth credentials"));
    }

    #[tokio::test]
    async fn the_key_never_appears_on_a_command_line() {
        // A stand-in curl that records its arguments and stdin.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        std::fs::write(
            dir.path().join("curl"),
            format!(
                "#!/bin/sh\necho \"ARGS $*\" > {log}\ncat >> {log}\necho '{{\"choices\":[{{\"message\":{{\"content\":\"{{\\\"ok\\\":true}}\"}}}}]}}'\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            dir.path().join("curl"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let mut env = EnvMap::new();
        env.insert(
            "PATH".into(),
            format!("{}:/usr/bin:/bin", dir.path().display()),
        );
        env.insert("OPENROUTER_API_KEY".into(), "sk-or-secret".into());
        let v = structured(&env, "Is it ok?", &json!({"type": "object"}), None)
            .await
            .unwrap();
        assert_eq!(v, json!({"ok": true}));
        let seen = std::fs::read_to_string(&log).unwrap();
        let args = seen.lines().next().unwrap();
        assert!(!args.contains("sk-or-secret"), "{args}");
        assert!(
            seen.contains("Authorization: Bearer sk-or-secret"),
            "on stdin"
        );
    }

    /// Against the real API, with the key in the environment:
    /// `cargo test -p otterd --bin otterd live_openrouter -- --ignored`.
    #[tokio::test]
    #[ignore = "calls OpenRouter; needs OPENROUTER_API_KEY"]
    async fn live_openrouter() {
        let mut env = EnvMap::new();
        env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        let schema = json!({
            "type": "object",
            "properties": {"sum": {"type": "integer"}},
            "required": ["sum"],
            "additionalProperties": false
        });
        let v = structured(&env, "What is 2 + 3?", &schema, None)
            .await
            .unwrap();
        assert_eq!(v["sum"], 5, "{v}");
    }
}
