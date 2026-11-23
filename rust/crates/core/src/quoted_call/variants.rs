//! The texts a block may hide in: decoded copies of a text and views of it
//! without per-line prefixes. Each copy is built only when its trigger is in
//! the text, in one pass, so the work stays linear in the text's length.

use std::borrow::Cow;

/// Whether `text` holds a block marker as a server reads it.
pub(super) fn has_marker(text: &str) -> bool {
    text.contains("<tool_call") || text.contains("<function=")
}

/// Whether `text` may hold a block in a form decoded here: escapes and
/// entities encode a marker's brackets, never its words. Most output has
/// neither word and is not decoded or parsed at all.
pub(super) fn may_hold_block(text: &str) -> bool {
    text.contains("tool_call") || text.contains("function=")
}

/// `text` as written, with JSON string escapes decoded (an oversized result
/// shows its data stringified and cut), with HTML entities decoded (a page
/// or a search snippet escapes markup), and with both: only the copies that
/// hold a marker once decoded.
pub(super) fn decoded(text: &str) -> Vec<Cow<'_, str>> {
    if !may_hold_block(text) {
        return vec![];
    }
    let plain = lf(Cow::Borrowed(text));
    let unescaped = json_unescaped(&plain).map(|text| lf(Cow::Owned(text)));
    let plain_entities = entities(&plain).map(Cow::Owned);
    let unescaped_entities = unescaped.as_deref().and_then(entities).map(Cow::Owned);
    [Some(plain), unescaped, plain_entities, unescaped_entities]
        .into_iter()
        .flatten()
        .filter(|text| has_marker(text))
        .collect()
}

/// `text` without the line prefixes of a file listing (`12\t`), a search
/// listing (`12: `), a unified diff (`+`, `-` or a space) and a markdown
/// quote (`> `): one view per prefix that some line has.
pub(super) fn line_views(text: &str) -> impl Iterator<Item = String> + '_ {
    [tab_numbered, colon_numbered, diff_marked, quote_marked]
        .into_iter()
        .filter_map(|strip| stripped(text, strip))
}

/// A server reads `\r\n` as `\n`; values are compared the same way.
pub(super) fn lf(text: Cow<'_, str>) -> Cow<'_, str> {
    if text.contains("\r\n") {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        text
    }
}

/// `text` with `strip` applied to each run of lines that carry its prefix
/// (blank lines may sit inside a run) and hold a block marker, or `None`
/// when no run does. A prefix counts only where a block's own lines carry
/// it: a value line `-5` in an unprefixed block is not read as `5`.
fn stripped(text: &str, strip: fn(&str) -> Option<&str>) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if !lines.iter().any(|line| strip(line).is_some_and(has_marker)) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < lines.len() {
        if strip(lines[at]).is_none() {
            out.push_str(lines[at]);
            at += 1;
            continue;
        }
        let run = lines[at..]
            .iter()
            .take_while(|line| strip(line).is_some() || line.trim().is_empty())
            .count();
        let run = &lines[at..at + run];
        let marked = run.iter().any(|line| strip(line).is_some_and(has_marker));
        for line in run {
            out.push_str(if marked {
                strip(line).unwrap_or(line)
            } else {
                line
            });
        }
        at += run.len();
    }
    Some(out)
}

/// The line after `spaces + digits`, when the line starts so.
fn after_number(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(' ');
    let after = rest.trim_start_matches(|ch: char| ch.is_ascii_digit());
    (after.len() < rest.len()).then_some(after)
}

fn tab_numbered(line: &str) -> Option<&str> {
    after_number(line)?.strip_prefix('\t')
}

fn colon_numbered(line: &str) -> Option<&str> {
    let rest = after_number(line)?.strip_prefix(':')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

fn diff_marked(line: &str) -> Option<&str> {
    line.strip_prefix(['+', '-', ' '])
}

fn quote_marked(line: &str) -> Option<&str> {
    line.strip_prefix("> ")
}

/// `text` with JSON string escapes decoded, or `None` when it has none.
fn json_unescaped(text: &str) -> Option<String> {
    if !text.contains('\\') {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let escape = &rest[at + 1..];
        let (decoded, used) = match escape.as_bytes().first() {
            Some(b'n') => ('\n', 1),
            Some(b't') => ('\t', 1),
            Some(b'r') => ('\r', 1),
            Some(b'"') => ('"', 1),
            Some(b'\\') => ('\\', 1),
            Some(b'/') => ('/', 1),
            Some(b'u' | b'U') => escape
                .get(1..5)
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .and_then(char::from_u32)
                .map_or(('\\', 0), |ch| (ch, 5)),
            _ => ('\\', 0),
        };
        out.push(decoded);
        changed |= used > 0;
        rest = &escape[used..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// `text` with the entities markup is escaped with decoded (`&lt;`, `&gt;`,
/// `&amp;`, `&quot;` and numeric forms of those characters and of `'`), or
/// `None` when it has none. One pass: `&amp;lt;` stays `&lt;`.
fn entities(text: &str) -> Option<String> {
    if !text.contains('&') {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match entity(tail) {
            Some((ch, used)) => {
                out.push(ch);
                changed = true;
                rest = &tail[used..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// The character the entity at the start of `tail` stands for, and the
/// entity's length. Names are read in any letter case.
fn entity(tail: &str) -> Option<(char, usize)> {
    let semicolon = tail.bytes().take(10).position(|byte| byte == b';')?;
    let name = &tail[1..semicolon];
    let ch = match name.to_ascii_lowercase().as_str() {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "quot" => '"',
        numeric => {
            let digits = numeric.strip_prefix('#')?;
            let code = match digits.strip_prefix('x') {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            match code {
                34 => '"',
                38 => '&',
                39 => '\'',
                60 => '<',
                62 => '>',
                _ => return None,
            }
        }
    };
    Some((ch, semicolon + 1))
}
