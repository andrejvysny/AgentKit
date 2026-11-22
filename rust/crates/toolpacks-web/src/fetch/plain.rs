//! Linear-time helpers for pathological HTML: nesting depth estimate and a
//! tag-stripping plain-text fallback (htmd recurses per nesting level).

use super::extract::decode_entities;

/// Elements without a closing tag; they never nest.
const VOID: [&str; 14] = [
    "br", "img", "hr", "input", "meta", "link", "area", "base", "col", "embed", "source", "track",
    "wbr", "param",
];
/// Elements whose end tag is optional; counting them would overestimate depth.
const OPTIONAL_END: [&str; 10] = [
    "p", "li", "dt", "dd", "tr", "td", "th", "option", "thead", "tbody",
];
const BLOCK: [&str; 22] = [
    "div",
    "p",
    "br",
    "li",
    "ul",
    "ol",
    "tr",
    "table",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "main",
    "header",
    "footer",
    "pre",
    "blockquote",
    "hr",
];

struct Tag<'a> {
    name: &'a str,
    closing: bool,
    self_closing: bool,
    /// Byte index just past the `>` (or the end of input).
    end: usize,
}

/// Parses the tag opening at `html[at] == '<'`; None for text, comments,
/// doctypes and processing instructions.
fn tag_at(html: &str, at: usize) -> Option<Tag<'_>> {
    let bytes = html.as_bytes();
    let closing = bytes.get(at + 1) == Some(&b'/');
    let name_start = at + 1 + usize::from(closing);
    let name_len = bytes[name_start.min(bytes.len())..]
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric())
        .count();
    if name_len == 0 || !bytes[name_start].is_ascii_alphabetic() {
        return None;
    }
    let name_end = name_start + name_len;
    let end = html[name_end..]
        .find('>')
        .map_or(html.len(), |p| name_end + p + 1);
    Some(Tag {
        name: &html[name_start..name_end],
        closing,
        self_closing: html[..end].ends_with("/>"),
        end,
    })
}

fn is_one_of(name: &str, list: &[&str]) -> bool {
    list.iter().any(|t| name.eq_ignore_ascii_case(t))
}

/// Approximate maximum tag nesting depth, in one pass.
pub fn max_depth(html: &str) -> usize {
    let (mut depth, mut max, mut at) = (0usize, 0usize, 0usize);
    while let Some(p) = html[at..].find('<') {
        let Some(tag) = tag_at(html, at + p) else {
            at += p + 1;
            continue;
        };
        at = tag.end;
        if is_one_of(tag.name, &VOID) || is_one_of(tag.name, &OPTIONAL_END) || tag.self_closing {
            continue;
        }
        if tag.closing {
            depth = depth.saturating_sub(1);
        } else {
            depth += 1;
            max = max.max(depth);
        }
    }
    max
}

/// Strips every tag (dropping script/style bodies), decodes basic entities and
/// joins the non-empty lines as paragraphs.
pub fn strip_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut text = String::with_capacity(html.len() / 2);
    let mut at = 0;
    while let Some(p) = html[at..].find('<') {
        text.push_str(&html[at..at + p]);
        let Some(tag) = tag_at(html, at + p) else {
            text.push('<');
            at += p + 1;
            continue;
        };
        at = tag.end;
        text.push(if is_one_of(tag.name, &BLOCK) {
            '\n'
        } else {
            ' '
        });
        let raw = ["script", "style"]
            .into_iter()
            .find(|n| tag.name.eq_ignore_ascii_case(n));
        if let (Some(name), false) = (raw, tag.closing) {
            at = lower[at..]
                .find(&format!("</{name}"))
                .map_or(html.len(), |q| at + q);
        }
    }
    text.push_str(&html[at..]);
    paragraphs(&decode_entities(&text))
}

fn paragraphs(text: &str) -> String {
    text.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}
