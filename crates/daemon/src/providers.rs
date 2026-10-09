//! Model providers for the Control Agent (D-045, D-052): one structured
//! answer per call, plain text streamed for replies, and the models each
//! provider offers.
//!
//! Most providers speak OpenAI's chat completions (OpenRouter, OpenAI,
//! Google Gemini, DeepSeek, xAI, Mistral, Groq); Anthropic speaks its own
//! Messages API. Each takes an API key, set in the host's Settings (D-048) or
//! in otterd's environment under the provider's variable. A key never goes on
//! a command line, into an event, or into a feature: requests go through
//! `curl` with the key in its config on stdin and the body in a private
//! temporary file. The model comes from Settings or `OTTER_CONTROLLER_MODEL`,
//! else the provider's default (some have none: pick one in Settings).

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use otter_protocol::host::ModelInfo;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::env::{EnvMap, which};

/// How a provider is spoken to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// `POST {base}/chat/completions`, `Authorization: Bearer`.
    OpenAi,
    /// `POST {base}/messages`, `x-api-key` (Anthropic's Messages API).
    Anthropic,
}

/// How a provider is asked for JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonMode {
    /// A JSON schema (`response_format` / `output_config.format`).
    Schema,
    /// Only "a JSON object": the schema goes in the prompt alone.
    Object,
}

#[derive(Debug)]
pub struct Provider {
    /// The controller's name in Settings and `OTTER_CONTROLLER`.
    pub id: &'static str,
    pub label: &'static str,
    /// Where its key lives: the secret's name, and the environment variable.
    pub key: &'static str,
    pub base: &'static str,
    pub style: Style,
    pub json: JsonMode,
    /// The model used when none is chosen (`None`: one must be chosen).
    pub default_model: Option<&'static str>,
}

/// In the order "automatic" tries them: the first with a key is used.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "openrouter",
        label: "OpenRouter",
        key: "OPENROUTER_API_KEY",
        base: "https://openrouter.ai/api/v1",
        style: Style::OpenAi,
        json: JsonMode::Schema,
        default_model: Some("openrouter/auto"),
    },
    Provider {
        id: "anthropic",
        label: "Anthropic",
        key: "ANTHROPIC_API_KEY",
        base: "https://api.anthropic.com/v1",
        style: Style::Anthropic,
        json: JsonMode::Schema,
        default_model: Some("claude-opus-5-5"),
    },
    Provider {
        id: "openai",
        label: "OpenAI",
        key: "OPENAI_API_KEY",
        base: "https://api.openai.com/v1",
        style: Style::OpenAi,
        json: JsonMode::Schema,
        default_model: None,
    },
    Provider {
        id: "gemini",
        label: "Google Gemini",
        key: "GEMINI_API_KEY",
        base: "https://generativelanguage.googleapis.com/v1beta/openai",
        style: Style::OpenAi,
        json: JsonMode::Schema,
        default_model: None,
    },
    Provider {
        id: "deepseek",
        label: "DeepSeek",
        key: "DEEPSEEK_API_KEY",
        base: "https://api.deepseek.com",
        style: Style::OpenAi,
        json: JsonMode::Object,
        default_model: Some("deepseek-chat"),
    },
    Provider {
        id: "xai",
        label: "xAI",
        key: "XAI_API_KEY",
        base: "https://api.x.ai/v1",
        style: Style::OpenAi,
        json: JsonMode::Schema,
        default_model: None,
    },
    Provider {
        id: "mistral",
        label: "Mistral",
        key: "MISTRAL_API_KEY",
        base: "https://api.mistral.ai/v1",
        style: Style::OpenAi,
        json: JsonMode::Schema,
        default_model: Some("mistral-large-latest"),
    },
    Provider {
        id: "groq",
        label: "Groq",
        key: "GROQ_API_KEY",
        base: "https://api.groq.com/openai/v1",
        style: Style::OpenAi,
        json: JsonMode::Object,
        default_model: None,
    },
];

/// The provider a controller name stands for.
pub fn get(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Claude Code's model names (`claude -p --model`): aliases first.
pub const CLAUDE_CODE_MODELS: &[(&str, &str)] = &[
    ("opus", "Opus (latest)"),
    ("sonnet", "Sonnet (latest)"),
    ("haiku", "Haiku (latest)"),
    ("claude-opus-5-5", "Claude Opus 5.5"),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
    ("claude-haiku-5-5", "Claude Haiku 5.5"),
    ("claude-fable-5-1", "Claude Fable 5.1"),
];

const ANTHROPIC_VERSION: &str = "2023-06-01";

impl Provider {
    /// Its key, from otterd's environment.
    pub fn env_key(&self, env: &EnvMap) -> Option<String> {
        env.get(self.key)
            .cloned()
            .or_else(|| std::env::var(self.key).ok())
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
    }

    fn model(&self, chosen: Option<&str>) -> Result<String> {
        chosen
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .or(self.default_model)
            .map(String::from)
            .ok_or_else(|| anyhow!("choose a model for {} in Settings", self.label))
    }

    /// curl's config: the key and the headers. Read from stdin, never argv.
    fn config(&self, key: &str) -> String {
        let mut c = String::from("header = \"Content-Type: application/json\"\n");
        match self.style {
            Style::OpenAi => c.push_str(&format!("header = \"Authorization: Bearer {key}\"\n")),
            Style::Anthropic => c.push_str(&format!(
                "header = \"x-api-key: {key}\"\nheader = \"anthropic-version: {ANTHROPIC_VERSION}\"\n"
            )),
        }
        if self.id == "openrouter" {
            c.push_str("header = \"X-Title: Otter\"\n");
        }
        c
    }

    fn endpoint(&self) -> String {
        match self.style {
            Style::OpenAi => format!("{}/chat/completions", self.base),
            Style::Anthropic => format!("{}/messages", self.base),
        }
    }

    /// The request: the prompt (plus an image, for a visual review), asking
    /// for JSON that fits `schema`.
    pub fn request_body(
        &self,
        model: &str,
        prompt: &str,
        schema: &Value,
        png: Option<&[u8]>,
    ) -> Value {
        let text = format!(
            "{prompt}\n\nAnswer with one JSON object only, matching this JSON schema:\n{schema}"
        );
        let image = png.map(|b| base64::engine::general_purpose::STANDARD.encode(b));
        match self.style {
            Style::OpenAi => {
                let content = match image {
                    Some(data) => json!([
                        {"type": "text", "text": text},
                        {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{data}")}}
                    ]),
                    None => json!(text),
                };
                let format = match self.json {
                    JsonMode::Schema => json!({
                        "type": "json_schema",
                        "json_schema": {"name": "answer", "schema": schema}
                    }),
                    JsonMode::Object => json!({"type": "json_object"}),
                };
                json!({
                    "model": model,
                    "messages": [{"role": "user", "content": content}],
                    "response_format": format
                })
            }
            Style::Anthropic => {
                let mut content = vec![json!({"type": "text", "text": text})];
                if let Some(data) = image {
                    content.insert(
                        0,
                        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}}),
                    );
                }
                json!({
                    "model": model,
                    "max_tokens": 16000,
                    "messages": [{"role": "user", "content": content}],
                    "output_config": {"format": {"type": "json_schema", "schema": closed(schema)}}
                })
            }
        }
    }

    /// The text of a whole (not streamed) answer.
    fn answer_text(&self, response: &Value) -> Result<String> {
        if let Some(e) = response.get("error") {
            bail!(
                "{}: {}",
                self.label,
                e["message"].as_str().unwrap_or("request failed")
            );
        }
        let text = match self.style {
            Style::OpenAi => response["choices"][0]["message"]["content"]
                .as_str()
                .map(String::from),
            Style::Anthropic => response["content"].as_array().map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect()
            }),
        };
        text.filter(|t| !t.trim().is_empty())
            .ok_or_else(|| anyhow!("{} gave no answer", self.label))
    }

    /// The JSON answer (models that ignore the format may wrap it in prose
    /// or a code fence).
    pub fn parse(&self, response: &Value) -> Result<Value> {
        let text = self.answer_text(response)?;
        if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
            return Ok(v);
        }
        let (start, end) = (text.find('{'), text.rfind('}'));
        match (start, end) {
            (Some(s), Some(e)) if e > s => serde_json::from_str(&text[s..=e])
                .with_context(|| format!("reading {}'s answer", self.label)),
            _ => bail!("{}'s answer isn't JSON", self.label),
        }
    }

    /// One structured answer. `image`: a PNG to look at.
    pub async fn structured(
        &self,
        env: &EnvMap,
        key: &str,
        model: Option<&str>,
        prompt: &str,
        schema: &Value,
        image: Option<&Path>,
    ) -> Result<Value> {
        let png = match image {
            Some(p) => Some(std::fs::read(p).with_context(|| format!("reading {}", p.display()))?),
            None => None,
        };
        let body = self.request_body(&self.model(model)?, prompt, schema, png.as_deref());
        let curl = self
            .curl(env, key, &self.endpoint(), Some(&body), false)
            .await?;
        let out = curl.child.wait_with_output().await?;
        if !out.status.success() {
            bail!(
                "reaching {}: {}",
                self.label,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("reading {}'s response", self.label))?;
        self.parse(&v)
    }

    /// Plain text from a model, streamed (server-sent events): `say` gets
    /// each piece as it's written; the whole text is returned (D-051).
    pub async fn stream_text(
        &self,
        env: &EnvMap,
        key: &str,
        model: Option<&str>,
        prompt: &str,
        say: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<String> {
        use tokio::io::AsyncBufReadExt;
        let model = self.model(model)?;
        let mut body = json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": true,
        });
        if self.style == Style::Anthropic {
            body["max_tokens"] = json!(16000);
        }
        let Curl {
            mut child,
            _body: _kept,
        } = self
            .curl(env, key, &self.endpoint(), Some(&body), true)
            .await?;
        let mut lines = tokio::io::BufReader::new(child.stdout.take().expect("piped")).lines();
        let mut text = String::new();
        let mut other = String::new();
        while let Some(line) = lines.next_line().await? {
            match sse_piece(&line) {
                Sse::Piece(t) => {
                    text.push_str(&t);
                    say(&t);
                }
                Sse::Done => break,
                Sse::Error(e) => bail!("{}: {e}", self.label),
                Sse::Other => other.push_str(&line),
            }
        }
        let out = child.wait_with_output().await?;
        if text.is_empty() {
            // Not a stream: an error answered as plain JSON, or curl failed.
            if let Ok(v) = serde_json::from_str::<Value>(&other) {
                self.answer_text(&v)?;
            }
            if !out.status.success() {
                bail!(
                    "reaching {}: {}",
                    self.label,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            bail!("{} gave no answer", self.label);
        }
        Ok(text)
    }

    /// The models it offers, as it lists them (chat models only, where it
    /// lists others too). OpenRouter's list needs no key.
    pub async fn models(&self, env: &EnvMap, key: Option<&str>) -> Result<Vec<ModelInfo>> {
        let url = match self.style {
            Style::OpenAi => format!("{}/models", self.base),
            Style::Anthropic => format!("{}/models?limit=1000", self.base),
        };
        let key = match key {
            Some(k) => k,
            None if self.id == "openrouter" => "",
            None => bail!("set the {} key ({}) first", self.label, self.key),
        };
        let curl = self.curl(env, key, &url, None, false).await?;
        let out = curl.child.wait_with_output().await?;
        if !out.status.success() {
            bail!(
                "reaching {}: {}",
                self.label,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let v: Value = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("reading {}'s model list", self.label))?;
        self.read_models(&v)
    }

    pub fn read_models(&self, v: &Value) -> Result<Vec<ModelInfo>> {
        if let Some(e) = v.get("error") {
            bail!(
                "{}: {}",
                self.label,
                e["message"].as_str().unwrap_or("request failed")
            );
        }
        let list = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("{} listed no models", self.label))?;
        Ok(list
            .iter()
            .filter_map(|m| {
                // Gemini lists `models/gemini-…`; chat takes `gemini-…`.
                let id = m["id"].as_str()?.trim_start_matches("models/").to_owned();
                let name = m["display_name"]
                    .as_str()
                    .or_else(|| m["name"].as_str())
                    .filter(|n| *n != id)
                    .map(String::from);
                Some(ModelInfo { id, name })
            })
            .filter(|m| chat_model(&m.id))
            .collect())
    }

    /// curl, with the key and headers in its config on stdin. `body`: POST
    /// it (from a private file); else GET.
    async fn curl(
        &self,
        env: &EnvMap,
        key: &str,
        url: &str,
        body: Option<&Value>,
        stream: bool,
    ) -> Result<Curl> {
        let curl = which("curl", env).ok_or_else(|| anyhow!("curl is not installed"))?;
        let config = if key.is_empty() {
            String::new()
        } else {
            self.config(key)
        };
        let mut cmd = tokio::process::Command::new(&curl);
        cmd.args([
            "-sS",
            "--max-time",
            if body.is_some() { "300" } else { "30" },
        ]);
        if stream {
            cmd.arg("-N");
        }
        cmd.args(["--config", "-"]);
        let file = match body {
            Some(b) => {
                let mut f = tempfile::Builder::new().prefix("otter-llm").tempfile()?;
                f.write_all(b.to_string().as_bytes())?;
                cmd.arg("--data-binary")
                    .arg(format!("@{}", f.path().display()));
                Some(f)
            }
            None => None,
        };
        cmd.arg(url)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = crate::env::spawn_tokio(&mut cmd)
            .await
            .context("starting curl")?;
        let mut stdin = child.stdin.take().expect("piped");
        stdin.write_all(config.as_bytes()).await?;
        drop(stdin);
        Ok(Curl { child, _body: file })
    }
}

/// A running curl, and the body file it reads (removed when this goes).
struct Curl {
    child: tokio::process::Child,
    _body: Option<tempfile::NamedTempFile>,
}

/// Not a chat model: embeddings, speech, images, moderation.
fn chat_model(id: &str) -> bool {
    const NOT: &[&str] = &[
        "embed",
        "tts",
        "whisper",
        "dall-e",
        "moderation",
        "transcribe",
        "realtime",
        "-audio",
        "image",
        "imagen",
        "veo",
        "davinci",
        "babbage",
        "guard",
    ];
    let id = id.to_lowercase();
    !NOT.iter().any(|n| id.contains(n))
}

/// A schema with every object closed (`additionalProperties: false`), as
/// Anthropic's structured outputs require.
fn closed(schema: &Value) -> Value {
    match schema {
        Value::Object(o) => {
            let mut o: serde_json::Map<String, Value> =
                o.iter().map(|(k, v)| (k.clone(), closed(v))).collect();
            if o.get("type") == Some(&json!("object")) && !o.contains_key("additionalProperties") {
                o.insert("additionalProperties".into(), json!(false));
            }
            Value::Object(o)
        }
        Value::Array(a) => Value::Array(a.iter().map(closed).collect()),
        v => v.clone(),
    }
}

#[derive(Debug, PartialEq)]
enum Sse {
    Piece(String),
    Done,
    Error(String),
    Other,
}

/// One line of an event stream, either style.
fn sse_piece(line: &str) -> Sse {
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return Sse::Other; // `event:` lines, comments, blank lines.
    };
    if data == "[DONE]" {
        return Sse::Done;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return Sse::Other;
    };
    if let Some(e) = v.get("error") {
        return Sse::Error(e["message"].as_str().unwrap_or("request failed").to_owned());
    }
    match v["type"].as_str() {
        // Anthropic: text deltas (not thinking), and the end.
        Some("message_stop") => return Sse::Done,
        Some("content_block_delta") => {
            return match v["delta"]["text"].as_str() {
                Some(t) if v["delta"]["type"] == "text_delta" && !t.is_empty() => {
                    Sse::Piece(t.to_owned())
                }
                _ => Sse::Other,
            };
        }
        _ => {}
    }
    match v["choices"][0]["delta"]["content"].as_str() {
        Some(t) if !t.is_empty() => Sse::Piece(t.to_owned()),
        _ => Sse::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn openrouter() -> &'static Provider {
        get("openrouter").unwrap()
    }

    #[test]
    fn event_stream_lines() {
        assert_eq!(
            sse_piece(r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#),
            Sse::Piece("Hel".into())
        );
        assert_eq!(sse_piece("data: [DONE]"), Sse::Done);
        assert_eq!(sse_piece(": OPENROUTER PROCESSING"), Sse::Other);
        assert_eq!(sse_piece(""), Sse::Other);
        assert_eq!(
            sse_piece(r#"data: {"error":{"message":"No auth credentials found"}}"#),
            Sse::Error("No auth credentials found".into())
        );
        // Anthropic's.
        assert_eq!(sse_piece("event: content_block_delta"), Sse::Other);
        assert_eq!(
            sse_piece(
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi"}}"#
            ),
            Sse::Piece("Hi".into())
        );
        assert_eq!(
            sse_piece(
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#
            ),
            Sse::Other
        );
        assert_eq!(sse_piece(r#"data: {"type":"message_stop"}"#), Sse::Done);
        assert_eq!(
            sse_piece(
                r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            ),
            Sse::Error("Overloaded".into())
        );
    }

    #[test]
    fn requests_ask_for_schema_json_and_carry_images() {
        let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let p = openrouter();
        let b = p.request_body("m/x", "Judge it.", &schema, None);
        assert_eq!(b["model"], "m/x");
        assert_eq!(b["response_format"]["json_schema"]["schema"], schema);
        assert!(
            b["messages"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("Judge it.")
        );
        let b = p.request_body("m/x", "Look.", &schema, Some(b"\x89PNG"));
        assert_eq!(b["messages"][0]["content"][1]["type"], "image_url");
        assert!(
            b["messages"][0]["content"][1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        // JSON-object-only providers.
        let b = get("deepseek")
            .unwrap()
            .request_body("deepseek-chat", "x", &schema, None);
        assert_eq!(b["response_format"], json!({"type": "json_object"}));
        // Anthropic: its own shape, objects closed, the image first.
        let b = get("anthropic").unwrap().request_body(
            "claude-opus-5-5",
            "Look.",
            &schema,
            Some(b"\x89PNG"),
        );
        let s = &b["output_config"]["format"]["schema"];
        assert_eq!(s["additionalProperties"], false);
        assert_eq!(b["messages"][0]["content"][0]["type"], "image");
        assert_eq!(b["messages"][0]["content"][1]["type"], "text");
        assert!(b["max_tokens"].as_u64().is_some());
    }

    #[test]
    fn answers_are_read_even_when_wrapped() {
        let p = openrouter();
        let r = |text: &str| json!({"choices": [{"message": {"content": text}}]});
        assert_eq!(p.parse(&r(r#"{"ok": true}"#)).unwrap(), json!({"ok": true}));
        assert_eq!(
            p.parse(&r("Here:\n```json\n{\"ok\": false}\n```")).unwrap(),
            json!({"ok": false})
        );
        assert!(p.parse(&r("no idea")).is_err());
        let err = p
            .parse(&json!({"error": {"message": "No auth credentials found"}}))
            .unwrap_err();
        assert!(format!("{err}").contains("No auth credentials"));
        // Anthropic: text blocks after thinking.
        let a = get("anthropic").unwrap();
        let v = json!({"content": [
            {"type": "thinking", "thinking": "…"},
            {"type": "text", "text": "{\"ok\": true}"}
        ]});
        assert_eq!(a.parse(&v).unwrap(), json!({"ok": true}));
    }

    #[test]
    fn model_lists_keep_chat_models() {
        let v = json!({"data": [
            {"id": "openai/gpt-x", "name": "OpenAI: GPT X"},
            {"id": "text-embedding-3-large"},
            {"id": "models/gemini-pro", "display_name": "Gemini Pro"},
            {"id": "whisper-1"}
        ]});
        let m = openrouter().read_models(&v).unwrap();
        let ids: Vec<&str> = m.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["openai/gpt-x", "gemini-pro"]);
        assert_eq!(m[1].name.as_deref(), Some("Gemini Pro"));
        assert!(
            openrouter()
                .read_models(&json!({"error": {"message": "bad key"}}))
                .is_err()
        );
    }

    #[test]
    fn a_model_is_needed_where_there_is_no_default() {
        assert_eq!(openrouter().model(None).unwrap(), "openrouter/auto");
        assert_eq!(openrouter().model(Some(" x ")).unwrap(), "x");
        let err = get("openai").unwrap().model(Some("")).unwrap_err();
        assert!(format!("{err}").contains("choose a model"));
        // Every provider's key name is distinct.
        let mut keys: Vec<_> = PROVIDERS.iter().map(|p| p.key).collect();
        keys.dedup();
        assert_eq!(keys.len(), PROVIDERS.len());
    }

    /// A stand-in curl that records its arguments and stdin, then prints
    /// `answer`.
    fn fake_curl(dir: &Path, answer: &str) -> (EnvMap, std::path::PathBuf) {
        let log = dir.join("log");
        std::fs::write(
            dir.join("curl"),
            format!(
                "#!/bin/sh\necho \"ARGS $*\" > {log}\ncat >> {log}\ncat <<'EOF'\n{answer}\nEOF\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(dir.join("curl"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = EnvMap::new();
        env.insert("PATH".into(), format!("{}:/usr/bin:/bin", dir.display()));
        (env, log)
    }

    #[tokio::test]
    async fn the_key_never_appears_on_a_command_line() {
        let dir = tempfile::tempdir().unwrap();
        let (env, log) = fake_curl(
            dir.path(),
            r#"{"choices":[{"message":{"content":"{\"ok\":true}"}}]}"#,
        );
        let v = openrouter()
            .structured(
                &env,
                "sk-or-secret",
                None,
                "Is it ok?",
                &json!({"type": "object"}),
                None,
            )
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

        // Anthropic's key goes the same way, in its own header.
        let (env, log) = fake_curl(
            dir.path(),
            r#"{"data":[{"id":"claude-opus-5-5","display_name":"Claude Opus 5.5"}]}"#,
        );
        let m = get("anthropic")
            .unwrap()
            .models(&env, Some("sk-ant-secret"))
            .await
            .unwrap();
        assert_eq!(m[0].id, "claude-opus-5-5");
        let seen = std::fs::read_to_string(&log).unwrap();
        assert!(!seen.lines().next().unwrap().contains("sk-ant-secret"));
        assert!(seen.contains("x-api-key: sk-ant-secret"));
        assert!(seen.contains("anthropic-version: 2023-06-01"));
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
        let p = openrouter();
        let key = p.env_key(&env).expect("OPENROUTER_API_KEY");
        let v = p
            .structured(&env, &key, None, "What is 2 + 3?", &schema, None)
            .await
            .unwrap();
        assert_eq!(v["sum"], 5, "{v}");
        assert!(p.models(&env, None).await.unwrap().len() > 10);
    }
}
