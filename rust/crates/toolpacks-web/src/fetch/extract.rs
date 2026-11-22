//! Bytes to markdown: charset decoding, main-content heuristic, HTML to
//! markdown, JSON pretty-printing.

use encoding_rs::{Encoding, UTF_8};
use url::Url;

use super::client::FetchedBody;
use super::links;
use super::plain;
use crate::normalize::clean_text;

const MIN_MAIN_CHARS: usize = 200;
const META_SNIFF_BYTES: usize = 2048;
const TITLE_MAX_CHARS: usize = 200;
/// Deeper nesting than this skips the recursive htmd converter.
const MAX_HTML_DEPTH: usize = 400;
const NOISE_TAGS: [&str; 9] = [
    "script", "style", "noscript", "nav", "footer", "form", "svg", "iframe", "template",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPage {
    pub title: Option<String>,
    pub markdown: String,
}

/// Charset named by `<meta charset=..>` or `http-equiv` in the first 2 KiB.
fn meta_charset(bytes: &[u8]) -> Option<String> {
    let head =
        String::from_utf8_lossy(&bytes[..bytes.len().min(META_SNIFF_BYTES)]).to_ascii_lowercase();
    let at = head.find("charset")?;
    let value = head[at + "charset".len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start()
        .trim_start_matches(['"', '\'']);
    let end = value
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
        .unwrap_or(value.len());
    (end > 0).then(|| value[..end].to_owned())
}

/// Decodes with the header charset, else the meta charset (HTML), else UTF-8.
fn decode(body: &FetchedBody) -> String {
    let label = body.charset.clone().or_else(|| {
        (body.content_type.contains("html"))
            .then(|| meta_charset(&body.bytes))
            .flatten()
    });
    let encoding = label
        .and_then(|l| Encoding::for_label(l.as_bytes()))
        .unwrap_or(UTF_8);
    encoding.decode(&body.bytes).0.into_owned()
}

pub(super) fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn title_of(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let title = clean_text(&decode_entities(&html[start..end]), TITLE_MAX_CHARS);
    (!title.is_empty()).then_some(title)
}

/// Contents of the first `<main>` (else `<article>`) element, if any.
fn main_region(html: &str) -> Option<&str> {
    let lower = html.to_ascii_lowercase();
    ["main", "article"].into_iter().find_map(|tag| {
        let open = format!("<{tag}");
        let start = lower
            .match_indices(&open)
            .map(|(i, _)| i)
            .find(|&i| lower[i + open.len()..].starts_with(['>', ' ', '\n', '\t', '\r']))?;
        let close = format!("</{tag}>");
        let end = lower.rfind(&close)? + close.len();
        (end > start).then(|| &html[start..end])
    })
}

fn to_markdown(html: &str, skip_header: bool) -> String {
    let mut skip: Vec<&str> = NOISE_TAGS.to_vec();
    if skip_header {
        skip.push("header");
    }
    let options = htmd::options::Options {
        bullet_list_marker: htmd::options::BulletListMarker::Dash,
        ul_bullet_spacing: 1,
        ..htmd::options::Options::default()
    };
    let converter = htmd::HtmlToMarkdown::builder()
        .options(options)
        .skip_tags(skip)
        .build();
    converter.convert(html).unwrap_or_default()
}

fn html_page(html: &str, base: &Url) -> ExtractedPage {
    if plain::max_depth(html) > MAX_HTML_DEPTH {
        return ExtractedPage {
            title: title_of(html),
            markdown: plain::strip_to_text(html),
        };
    }
    let main = main_region(html).map(|region| to_markdown(region, false));
    let markdown = match main {
        Some(md) if md.chars().count() >= MIN_MAIN_CHARS => md,
        _ => to_markdown(html, true),
    };
    let markdown = links::collapse_blank_lines(&links::rewrite(&markdown, base));
    let title = title_of(html).or_else(|| {
        markdown
            .lines()
            .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_owned()))
    });
    ExtractedPage { title, markdown }
}

fn plain_page(text: String) -> ExtractedPage {
    ExtractedPage {
        title: None,
        markdown: text,
    }
}

pub fn extract(body: &FetchedBody) -> ExtractedPage {
    let text = decode(body);
    match body.content_type.as_str() {
        "text/html" | "application/xhtml+xml" => html_page(&text, &body.final_url),
        "application/json" => {
            let pretty = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok());
            plain_page(pretty.unwrap_or(text))
        }
        _ => plain_page(text),
    }
}
