//! Position-encoding bridge. The tool interface speaks 1-based lines and
//! 1-based columns counted in Unicode scalar values (what a model sees as
//! "characters"); LSP speaks 0-based lines and UTF-16 code units, the
//! encoding this client advertises in `general.positionEncodings`.

use std::sync::Arc;

/// Line starts of one document text, for O(log n) line lookup.
#[derive(Clone, Debug)]
pub struct LineIndex {
    text: Arc<str>,
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: Arc<str>) -> Self {
        let bytes = text.as_bytes();
        let mut starts = vec![0usize];
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => starts.push(index + 1),
                b'\r' => {
                    if bytes.get(index + 1) == Some(&b'\n') {
                        index += 1;
                    }
                    starts.push(index + 1);
                }
                _ => {}
            }
            index += 1;
        }
        Self { text, starts }
    }

    #[cfg(test)]
    fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// The line without its terminator, `None` past the last line.
    fn line(&self, line: u32) -> Option<&str> {
        let line = line as usize;
        let start = *self.starts.get(line)?;
        let end = self
            .starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        Some(self.text[start..end].trim_end_matches(['\n', '\r']))
    }

    /// 0-based scalar column → UTF-16 code-unit offset. A column past the
    /// end of the line clamps to the line end (the LSP rule). `None` when
    /// the line does not exist.
    pub fn char_to_utf16(&self, line: u32, character: u32) -> Option<u32> {
        let text = self.line(line)?;
        let units = text
            .chars()
            .take(character as usize)
            .map(|c| c.len_utf16())
            .sum::<usize>();
        Some(u32::try_from(units).unwrap_or(u32::MAX))
    }

    /// UTF-16 offset → 0-based scalar column; an offset inside a surrogate
    /// pair rounds down to the pair's start, one past the end clamps. An
    /// unknown line leaves the offset untouched.
    pub fn utf16_to_char(&self, line: u32, utf16: u32) -> u32 {
        let Some(text) = self.line(line) else {
            return utf16;
        };
        let mut units = 0u32;
        let mut chars = 0u32;
        for c in text.chars() {
            let width = c.len_utf16() as u32;
            if units + width > utf16 {
                break;
            }
            units += width;
            chars += 1;
        }
        chars
    }
}

/// A server-supplied LSP position as 1-based tool coordinates. Server
/// numbers are untrusted: negatives read as 0 and the `+ 1` saturates, so
/// a hostile `u64::MAX` can neither overflow nor panic. With a line index
/// the UTF-16 column becomes a scalar column; without one (a document we
/// could not read) the raw offset is reported.
pub fn lsp_to_tool(index: Option<&LineIndex>, point: &serde_json::Value) -> (u64, u64) {
    let unit = |key: &str| {
        point
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .min(u64::from(u32::MAX))
    };
    let (line, character) = (unit("line"), unit("character"));
    let character = match index {
        Some(index) => u64::from(index.utf16_to_char(line as u32, character as u32)),
        None => character,
    };
    (line.saturating_add(1), character.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(text: &str) -> LineIndex {
        LineIndex::new(Arc::from(text))
    }

    #[test]
    fn ascii_columns_are_identity() {
        let idx = index("let abc = 1;\nsecond");
        assert_eq!(idx.char_to_utf16(0, 4), Some(4));
        assert_eq!(idx.utf16_to_char(1, 3), 3);
        assert_eq!(idx.line_count(), 2);
    }

    #[test]
    fn bmp_non_ascii_is_one_unit_per_scalar() {
        let idx = index("é日本x");
        assert_eq!(idx.char_to_utf16(0, 3), Some(3));
        assert_eq!(idx.utf16_to_char(0, 3), 3);
    }

    #[test]
    fn emoji_takes_two_utf16_units() {
        // "a😀b": scalars a,😀,b -> UTF-16 units 1,2,1
        let idx = index("a😀b\nz");
        assert_eq!(idx.char_to_utf16(0, 2), Some(3), "column of `b`");
        assert_eq!(idx.utf16_to_char(0, 3), 2);
        // Mid-pair offset rounds down to the pair start.
        assert_eq!(idx.utf16_to_char(0, 2), 1);
        // Past the end clamps.
        assert_eq!(idx.char_to_utf16(0, 99), Some(4));
        assert_eq!(idx.utf16_to_char(0, 99), 3);
    }

    #[test]
    fn hostile_server_numbers_saturate() {
        use serde_json::json;
        let point = json!({"line": u64::MAX, "character": u64::MAX});
        assert_eq!(
            lsp_to_tool(None, &point),
            (u64::from(u32::MAX) + 1, u64::from(u32::MAX) + 1)
        );
        assert_eq!(lsp_to_tool(None, &json!({"line": -4})), (1, 1));
        let idx = index("a😀b");
        assert_eq!(
            lsp_to_tool(Some(&idx), &json!({"line":0,"character":3})),
            (1, 3)
        );
    }

    #[test]
    fn crlf_and_cr_terminators_split_lines() {
        let idx = index("ab\r\ncd\ref");
        assert_eq!(idx.line_count(), 3);
        assert_eq!(idx.char_to_utf16(1, 1), Some(1));
        assert_eq!(idx.char_to_utf16(2, 5), Some(2));
        assert_eq!(idx.char_to_utf16(3, 0), None);
    }
}
