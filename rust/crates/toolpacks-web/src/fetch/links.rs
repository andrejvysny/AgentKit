//! Post-processing of converted markdown: images become alt text, relative
//! link targets become absolute, links to non-web schemes are reduced to text.

use url::Url;

/// Longest destination (and title) a link may have; keeps every scan bounded.
const DEST_SCAN_BYTES: usize = 2048;

/// Remembers where the last failed destination scan stopped, so overlapping
/// `](` starts inside that stretch do not rescan it (keeps `rewrite` linear).
#[derive(Default)]
struct DeadZone {
    end: usize,
    at_newline: bool,
}

fn window_of(tail: &str) -> &str {
    let mut end = tail.len().min(DEST_SCAN_BYTES);
    while !tail.is_char_boundary(end) {
        end -= 1;
    }
    &tail[..end]
}

/// Splits the text after `](` into (destination, remainder after the closing
/// paren). Handles `<dest>`, backslash escapes and an optional `"title"`.
/// Scans at most [`DEST_SCAN_BYTES`] bytes and never crosses a newline;
/// `base` is the absolute offset of `tail`.
fn split_destination<'a>(
    tail: &'a str,
    base: usize,
    dead: &mut DeadZone,
) -> Option<(&'a str, &'a str)> {
    let mut from = 0;
    if base < dead.end {
        if dead.at_newline {
            return None;
        }
        from = dead.end - base;
    }
    let window = window_of(tail);
    let mut escaped = false;
    let scan = window.get(from..).unwrap_or_default();
    for (i, c) in scan.char_indices().map(|(i, c)| (i + from, c)) {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '\n' => {
                *dead = DeadZone {
                    end: base + i,
                    at_newline: true,
                };
                return None;
            }
            ')' => return Some((&tail[..i], &tail[i + 1..])),
            ' ' => {
                let close = window[i..].find("\")")?;
                return Some((&tail[..i], &tail[i + close + 2..]));
            }
            _ => {}
        }
    }
    *dead = DeadZone {
        end: base + window.len(),
        at_newline: false,
    };
    None
}

fn absolute(dest: &str, base: &Url) -> Option<String> {
    let dest = dest.trim_start_matches('<').trim_end_matches('>');
    let joined = base.join(dest).ok()?;
    matches!(joined.scheme(), "http" | "https" | "mailto")
        .then(|| joined.as_str().replace('(', "%28").replace(')', "%29"))
}

/// Appends `text` to `out`, recording the byte index of every `[` in it.
fn push_tracked(out: &mut String, opens: &mut Vec<usize>, text: &str) {
    let base = out.len();
    opens.extend(text.match_indices('[').map(|(i, _)| base + i));
    out.push_str(text);
}

/// Rewrites `![alt](src)` to `alt` and `[text](dest)` to an absolute link.
pub fn rewrite(md: &str, base: &Url) -> String {
    let mut out = String::with_capacity(md.len());
    let mut opens: Vec<usize> = Vec::new();
    let mut dead = DeadZone::default();
    let mut rest = md;
    while let Some(i) = rest.find("](") {
        push_tracked(&mut out, &mut opens, &rest[..i]);
        let tail = &rest[i + 2..];
        let split = opens
            .last()
            .and_then(|_| split_destination(tail, md.len() - tail.len(), &mut dead));
        let (Some(&open), Some((dest, after))) = (opens.last(), split) else {
            out.push_str("](");
            rest = tail;
            continue;
        };
        opens.pop();
        if open > 0 && out.as_bytes()[open - 1] == b'!' {
            let alt = out.split_off(open + 1);
            out.truncate(open - 1);
            out.push_str(&alt);
        } else if let Some(target) = absolute(dest, base) {
            opens.push(open);
            out.push_str("](");
            push_tracked(&mut out, &mut opens, &target);
            out.push(')');
        } else {
            out.remove(open);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Collapses runs of three or more blank lines to two.
pub fn collapse_blank_lines(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut blank = 0usize;
    for line in md.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 2 {
                continue;
            }
            out.push('\n');
        } else {
            blank = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/a/").unwrap()
    }

    #[test]
    fn hostile_input_is_linear_and_lossless() {
        let unit = "\\](x[a](";
        let input = unit.repeat(3 * 1024 * 1024 / unit.len());
        let started = Instant::now();
        let out = rewrite(&input, &base());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out, input);
    }

    #[test]
    fn unclosed_destination_keeps_text() {
        let md = format!("[a](https://x.test/{}", "p".repeat(5000));
        assert_eq!(rewrite(&md, &base()), md);
        assert_eq!(rewrite("[a](b\nc)", &base()), "[a](b\nc)");
    }

    #[test]
    fn rewrites_still_work() {
        assert_eq!(rewrite("[a](b)", &base()), "[a](https://example.com/a/b)");
        assert_eq!(rewrite("x ![alt](i.png) y", &base()), "x alt y");
        assert_eq!(rewrite("[a](javascript:x)", &base()), "a");
        assert_eq!(
            rewrite("[a ![b](c) d](e)", &base()),
            "[a b d](https://example.com/a/e)"
        );
    }
}
