//! Text an approval card shows that the model chose (a path, a title, file
//! content). Control characters, Unicode line and paragraph separators,
//! bidirectional controls and invisible format characters are written as
//! escapes, so such text can neither add a line to the card nor reorder or
//! hide what it shows. Escaping can only lengthen text, so every bound is
//! applied to the escaped form.

/// True for a character that breaks, reorders or hides card text.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

fn push_escaped(out: &mut String, c: char, backslash: bool) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\\' if backslash => out.push_str("\\\\"),
        c if hidden(c) => out.extend(c.escape_unicode()),
        c => out.push(c),
    }
}

/// `text` as one card field (a path, a title): everything hidden escaped,
/// and a backslash too, so the escaped form reads back unambiguously.
pub(crate) fn field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push_escaped(&mut out, c, true);
    }
    out
}

/// At most `max_lines` lines of `text` (file content) for the card's
/// preview, everything hidden in a line escaped, at most `max_chars`
/// characters in all. A backslash stays as it is: the preview is the last
/// section of the card and labelled as content.
pub(crate) fn preview(text: &str, max_lines: usize, max_chars: usize) -> String {
    let mut out = String::new();
    let mut left = max_chars;
    let mut escaped = String::new();
    for (n, line) in text.lines().take(max_lines).enumerate() {
        // A line holds no '\n', so the only one here separates lines.
        let separator = (n > 0).then_some('\n');
        for c in separator.into_iter().chain(line.chars()) {
            escaped.clear();
            if c == '\n' {
                escaped.push(c);
            } else {
                push_escaped(&mut escaped, c, false);
            }
            let cost = escaped.chars().count();
            if cost > left {
                return out;
            }
            left -= cost;
            out.push_str(&escaped);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hidden_character_is_escaped_and_ordinary_text_is_kept() {
        let text = "a\nb\rc\u{1b}d\u{202e}e\u{2066}f\u{2028}g\u{200b}h\u{feff}i\\j";
        assert_eq!(
            field(text),
            "a\\nb\\rc\\u{1b}d\\u{202e}e\\u{2066}f\\u{2028}g\\u{200b}h\\u{feff}i\\\\j"
        );
        for kept in ["report é.csv", "日本語", "naïve 🙂", "a b-c_d.txt"] {
            assert_eq!(field(kept), kept);
        }
    }

    #[test]
    fn a_preview_keeps_lines_escapes_inside_them_and_is_bounded() {
        assert_eq!(
            preview("one\u{202e}\ntwo\\x\nthree", 2, 100),
            "one\\u{202e}\ntwo\\x"
        );
        let wide = "\u{1b}".repeat(100);
        let bounded = preview(&wide, 20, 50);
        assert_eq!(bounded, "\\u{1b}".repeat(8), "whole escapes only");
        assert_eq!(preview("abc\ndef", 20, 3), "abc", "no dangling newline");
        assert_eq!(preview("abc\ndef", 20, 4), "abc\n");
        assert_eq!(preview("a\n\nb", 20, 100), "a\n\nb", "empty lines kept");
    }
}
