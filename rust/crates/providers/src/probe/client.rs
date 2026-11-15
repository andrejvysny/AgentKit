use futures_util::StreamExt;
use serde_json::Value;
use std::time::{Duration, Instant};

/// API key holder: `Debug` never prints it and `scrub` removes it from text.
#[derive(Clone)]
pub struct Secret(String);
impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn scrub(&self, text: &str) -> String {
        if self.0.is_empty() {
            text.to_string()
        } else {
            text.replace(&self.0, "[redacted]")
        }
    }
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
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
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
                    chunks.push(chunk);
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
