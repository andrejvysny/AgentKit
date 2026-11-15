use futures_util::StreamExt;
use serde_json::Value;
use std::time::{Duration, Instant};

/// API key holder: `Debug` never prints it and `scrub` removes it from text.
/// Scrubbing covers the raw key plus its JSON-escaped, URL-encoded and base64
/// forms, so an endpoint that echoes it in any of those shapes is still caught.
#[derive(Clone)]
pub struct Secret(String, Vec<String>);
impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let forms = secret_forms(&value);
        Self(value, forms)
    }
    pub fn scrub(&self, text: &str) -> String {
        self.1
            .iter()
            .fold(text.to_string(), |acc, form| acc.replace(form, "[redacted]"))
    }
    /// Scrubs every string (and object key) inside a JSON value.
    pub fn scrub_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.scrub(text)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.scrub_value(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (self.scrub(k), self.scrub_value(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// Distinct non-empty representations of the key, longest first.
fn secret_forms(key: &str) -> Vec<String> {
    if key.is_empty() {
        return Vec::new();
    }
    let json_escaped = serde_json::to_string(key)
        .map(|quoted| quoted[1..quoted.len() - 1].to_string())
        .unwrap_or_default();
    let mut forms = vec![
        key.to_string(),
        json_escaped,
        percent_encode(key),
        base64_encode(key.as_bytes()),
    ];
    forms.retain(|form| !form.is_empty());
    forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
    forms.dedup();
    forms
}

fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
    }
    out
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Base URL including the version prefix, for example `http://host:4000/v1`.
    pub base_url: String,
    pub model: String,
    pub key: Secret,
    http: reqwest::Client,
}

/// One HTTP exchange. `body` is `Null` when the reply was not JSON.
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub text: String,
}
impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    /// First choice's message of a non-streaming reply.
    pub fn message(&self) -> &Value {
        &self.body["choices"][0]["message"]
    }
}

/// A streamed reply: every JSON `data:` chunk plus time to the first chunk.
pub struct Streamed {
    pub status: u16,
    pub chunks: Vec<Value>,
    pub first_chunk: Option<Duration>,
    pub text: String,
}

impl Endpoint {
    pub fn new(base_url: &str, model: &str, key: Secret) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            key,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_default(),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self
            .http
            .request(method, format!("{}{path}", self.base_url));
        if self.key.0.is_empty() {
            request
        } else {
            request.bearer_auth(&self.key.0)
        }
    }

    /// Chat body with the model filled in; `extra` keys override.
    pub fn chat_body(&self, messages: Value, extra: Value) -> Value {
        let mut body = serde_json::json!({"model": self.model, "messages": messages});
        if let (Some(target), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        body
    }

    pub async fn chat(&self, body: &Value) -> Result<Reply, String> {
        self.send(
            self.request(reqwest::Method::POST, "/chat/completions")
                .json(body),
        )
        .await
    }

    pub async fn get(&self, path: &str) -> Result<Reply, String> {
        self.send(self.request(reqwest::Method::GET, path)).await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Reply, String> {
        let response = request
            .send()
            .await
            .map_err(|e| self.key.scrub(&e.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|e| self.key.scrub(&e.to_string()))?;
        let body = serde_json::from_str(&text)
            .map(|parsed| self.key.scrub_value(&parsed))
            .unwrap_or(Value::Null);
        Ok(Reply {
            status,
            body,
            text: self.key.scrub(&text),
        })
    }

    pub async fn chat_stream(&self, body: &Value) -> Result<Streamed, String> {
        let mut body = body.clone();
        body["stream"] = true.into();
        body["stream_options"] = serde_json::json!({"include_usage": true});
        let started = Instant::now();
        let response = self
            .request(reqwest::Method::POST, "/chat/completions")
            .json(&body)
            .send()
            .await
            .map_err(|e| self.key.scrub(&e.to_string()))?;
        let status = response.status().as_u16();
        let mut stream = response.bytes_stream();
        let (mut buffer, mut text) = (String::new(), String::new());
        let (mut chunks, mut first_chunk) = (Vec::new(), None);
        while let Some(part) = stream.next().await {
            let part = part.map_err(|e| self.key.scrub(&e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&part));
            while let Some(end) = buffer.find('\n') {
                let line: String = buffer.drain(..=end).collect();
                text.push_str(&line);
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                if let Ok(chunk) = serde_json::from_str::<Value>(data.trim()) {
                    first_chunk.get_or_insert_with(|| started.elapsed());
                    chunks.push(self.key.scrub_value(&chunk));
                }
            }
        }
        Ok(Streamed {
            status,
            chunks,
            first_chunk,
            text: self.key.scrub(&text),
        })
    }
}
