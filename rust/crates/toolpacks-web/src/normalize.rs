//! URL canonicalisation and text cleaning for search results.

use url::Url;

pub const TITLE_MAX: usize = 200;
pub const SNIPPET_MAX: usize = 320;
pub const ANSWER_MAX: usize = 400;
const URL_MAX_BYTES: usize = 2048;

const TRACKING_KEYS: [&str; 6] = ["gclid", "fbclid", "msclkid", "mc_cid", "mc_eid", "ref_src"];

fn is_tracking(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.starts_with("utm_") || TRACKING_KEYS.contains(&key.as_str())
}

/// http(s) only; userinfo, fragment and tracking params dropped; trailing slash trimmed.
pub fn canonical_url(raw: &str) -> Option<Url> {
    let mut url = Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let kept: Vec<&(String, String)> = pairs.iter().filter(|(k, _)| !is_tracking(k)).collect();
    if kept.len() != pairs.len() {
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut()
                .clear()
                .extend_pairs(kept.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
    }
    if url.path() != "/" && url.path().ends_with('/') {
        let trimmed = url.path().trim_end_matches('/');
        let path = if trimmed.is_empty() { "/" } else { trimmed }.to_owned();
        url.set_path(&path);
    }
    (url.as_str().len() <= URL_MAX_BYTES).then_some(url)
}

pub fn clean_text(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &collapsed[..end]),
        None => collapsed,
    }
}

/// Removes `<...>` sequences, then decodes the few entities search APIs emit.
/// Tags go first so a decoded `&lt;` is never mistaken for markup.
pub fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // `&amp;` last: decoding it earlier would double-decode `&amp;lt;`.
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

pub fn normalize_domain(raw: &str) -> Option<String> {
    let mut text = raw.trim().to_ascii_lowercase();
    for prefix in ["http://", "https://"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.to_owned();
        }
    }
    if let Some(end) = text.find('/') {
        text.truncate(end);
    }
    let text = text.strip_prefix("www.").unwrap_or(&text);
    let valid = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    valid.then(|| text.to_owned())
}

pub fn domain_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    host.strip_prefix("www.").unwrap_or(host).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(raw: &str) -> Option<String> {
        canonical_url(raw).map(String::from)
    }

    #[test]
    fn strips_tracking_fragment_and_slash() {
        assert_eq!(
            canon("https://Example.com:443/a/b/?utm_source=x&id=7&GCLID=1#top").as_deref(),
            Some("https://example.com/a/b?id=7")
        );
        assert_eq!(
            canon("https://example.com/?utm_medium=m").as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            canon("https://example.com/p/?fbclid=z").as_deref(),
            Some("https://example.com/p")
        );
    }

    #[test]
    fn strips_userinfo() {
        assert_eq!(
            canon("https://user:pw@example.com/a").as_deref(),
            Some("https://example.com/a")
        );
    }

    #[test]
    fn rejects_overlong_urls() {
        let long = format!("https://example.com/{}", "a".repeat(2100));
        assert!(canonical_url(&long).is_none());
        let ok = format!("https://example.com/{}", "a".repeat(2000));
        assert!(canonical_url(&ok).is_some());
    }

    #[test]
    fn rejects_non_http() {
        for bad in [
            "ftp://h/x",
            "javascript:alert(1)",
            "",
            "not a url",
            "mailto:a@b.c",
        ] {
            assert!(canonical_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn clean_text_collapses_and_clamps_on_char_boundary() {
        assert_eq!(clean_text("  a \n\t b   c ", 50), "a b c");
        let text = "žľščť".repeat(10);
        let out = clean_text(&text, 7);
        assert_eq!(out, "žľščťžľ…");
        assert_eq!(clean_text("abc", 3), "abc");
    }

    #[test]
    fn strip_tags_removes_markup_and_decodes_entities() {
        assert_eq!(
            strip_tags(
                "Tokio <strong>runtime</strong> &amp; more &lt;b&gt; &quot;x&quot; it&#39;s&nbsp;ok&#x27;"
            ),
            "Tokio runtime & more <b> \"x\" it's ok'"
        );
        assert_eq!(strip_tags("a &amp;lt; b"), "a &lt; b");
        assert_eq!(strip_tags("x <unclosed"), "x ");
        assert_eq!(strip_tags("a > b"), "a > b");
    }

    #[test]
    fn domain_normalization() {
        assert_eq!(
            normalize_domain(" HTTPS://www.Docs.RS/x/y ").as_deref(),
            Some("docs.rs")
        );
        assert_eq!(
            normalize_domain("blog.rust-lang.org").as_deref(),
            Some("blog.rust-lang.org")
        );
        assert!(normalize_domain("").is_none());
        assert!(normalize_domain("bad host").is_none());
        assert!(normalize_domain("a_b.com").is_none());
        let url = Url::parse("https://www.example.com/x").unwrap();
        assert_eq!(domain_of(&url), "example.com");
    }
}
