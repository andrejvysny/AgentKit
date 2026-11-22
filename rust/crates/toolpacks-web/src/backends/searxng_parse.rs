//! SearXNG JSON response parsing; every third-party string is clamped here.

use serde_json::Value;

use crate::backend::{BackendError, SearchHit};
use crate::normalize::{ANSWER_MAX, SNIPPET_MAX, TITLE_MAX, canonical_url, clean_text};

const MAX_SUGGESTIONS: usize = 3;
const PUBLISHED_MAX: usize = 40;
const ENGINE_NAME_MAX: usize = 40;
const MAX_UNRESPONSIVE: usize = 5;

/// One parsed SearXNG page, hits already canonicalised.
#[derive(Default)]
pub(super) struct Page {
    pub raw_count: usize,
    pub hits: Vec<SearchHit>,
    pub answer: Option<String>,
    pub suggestions: Vec<String>,
    pub unresponsive: Vec<String>,
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn build_hit(item: &Value) -> Option<SearchHit> {
    let url = canonical_url(str_field(item, "url")?)?;
    let engine = str_field(item, "engine")
        .filter(|e| !e.is_empty())
        .map_or_else(|| "searxng".to_owned(), |e| format!("searxng:{e}"));
    let extra = item
        .get("engines")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let published = str_field(item, "publishedDate")
        .map(|p| clean_text(p, PUBLISHED_MAX))
        .filter(|p| !p.is_empty());
    Some(SearchHit {
        title: clean_text(str_field(item, "title").unwrap_or(""), TITLE_MAX),
        snippet: clean_text(str_field(item, "content").unwrap_or(""), SNIPPET_MAX),
        published,
        extra,
        engine,
        url,
    })
}

fn first_answer(value: &Value) -> Option<String> {
    value.get("answers")?.as_array()?.iter().find_map(|a| {
        let text = a.as_str().or_else(|| str_field(a, "answer"))?;
        let text = clean_text(text, ANSWER_MAX);
        (!text.is_empty()).then_some(text)
    })
}

fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| clean_text(s, TITLE_MAX))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn unresponsive_names(value: &Value) -> Vec<String> {
    let Some(items) = value.get("unresponsive_engines").and_then(Value::as_array) else {
        return vec![];
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::Array(parts) => parts.first().and_then(Value::as_str),
            Value::String(name) => Some(name.as_str()),
            _ => None,
        })
        .map(|name| clean_text(name, ENGINE_NAME_MAX))
        .filter(|name| !name.is_empty())
        .take(MAX_UNRESPONSIVE)
        .collect()
}

pub(super) fn parse_page(body: &[u8]) -> Result<Page, BackendError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| BackendError::BadResponse("invalid JSON".into()))?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| BackendError::BadResponse("missing results".into()))?;
    let mut suggestions = string_list(&value, "suggestions");
    suggestions.truncate(MAX_SUGGESTIONS);
    Ok(Page {
        raw_count: results.len(),
        hits: results.iter().filter_map(build_hit).collect(),
        answer: first_answer(&value),
        suggestions,
        unresponsive: unresponsive_names(&value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse_string_and_object_forms() {
        let v: Value = serde_json::json!({"answers": ["  plain  answer "]});
        assert_eq!(first_answer(&v).as_deref(), Some("plain answer"));
        let v: Value = serde_json::json!({"answers": [{"answer": "obj", "url": "x"}]});
        assert_eq!(first_answer(&v).as_deref(), Some("obj"));
        let v: Value = serde_json::json!({"answers": [{"other": 1}, "second"]});
        assert_eq!(first_answer(&v).as_deref(), Some("second"));
        let long = "a".repeat(900);
        let v: Value = serde_json::json!({ "answers": [long] });
        assert_eq!(
            first_answer(&v).map(|a| a.chars().count()),
            Some(ANSWER_MAX + 1)
        );
    }

    #[test]
    fn published_is_single_line_and_clamped() {
        let item =
            serde_json::json!({"url": "https://a.com/x", "publishedDate": "2026-01-01\n[9] fake"});
        let hit = build_hit(&item).unwrap();
        assert_eq!(hit.published.as_deref(), Some("2026-01-01 [9] fake"));
        let item = serde_json::json!({"url": "https://a.com/x", "publishedDate": "d".repeat(100)});
        let hit = build_hit(&item).unwrap();
        assert_eq!(hit.published.map(|p| p.chars().count()), Some(41));
    }

    #[test]
    fn unresponsive_names_clamped_and_capped() {
        let names: Vec<String> = (0..8)
            .map(|i| format!("e{i}\n{}", "x".repeat(80)))
            .collect();
        let v = serde_json::json!({ "unresponsive_engines": names });
        let out = unresponsive_names(&v);
        assert_eq!(out.len(), 5);
        assert!(
            out.iter()
                .all(|n| !n.contains('\n') && n.chars().count() <= 41)
        );
    }

    #[test]
    fn unresponsive_shapes_tolerated() {
        let v = serde_json::json!({"unresponsive_engines": [["google","timeout"],"bing",7,[]]});
        assert_eq!(unresponsive_names(&v), vec!["google", "bing"]);
    }
}
