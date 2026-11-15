use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    /// A measurement or an accepted/rejected fact with no pass mark.
    Info,
    /// The check could not run (transport error before any answer).
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    pub id: &'static str,
    pub title: &'static str,
    /// What the answer decides, from the spec table.
    pub decides: &'static str,
    pub status: Status,
    pub detail: String,
    pub data: Value,
}
impl CheckResult {
    pub fn new(id: &'static str, title: &'static str, decides: &'static str) -> Self {
        Self {
            id,
            title,
            decides,
            status: Status::Error,
            detail: String::new(),
            data: Value::Null,
        }
    }
    pub fn with(mut self, status: Status, detail: impl Into<String>) -> Self {
        self.status = status;
        self.detail = detail.into();
        self
    }
    pub fn data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub base_url: String,
    pub model: String,
    pub results: Vec<CheckResult>,
}
impl Report {
    pub fn get(&self, id: &str) -> Option<&CheckResult> {
        self.results.iter().find(|result| result.id == id)
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
    pub fn to_markdown(&self) -> String {
        let mut out = format!(
            "# Endpoint probe\n\nEndpoint: `{}`  \nModel: `{}`\n\n| Check | Status | Decides | Detail |\n|---|---|---|---|\n",
            self.base_url, self.model
        );
        for r in &self.results {
            let detail = r.detail.replace('|', "\\|").replace('\n', " ");
            out.push_str(&format!(
                "| {} {} | {:?} | {} | {} |\n",
                r.id, r.title, r.status, r.decides, detail
            ));
        }
        out
    }
}
