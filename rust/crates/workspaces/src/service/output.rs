//! The bounded tail of a command's output (INV-9): what the caller sees is
//! the last lines and bytes of [`OutputLimit`], however much the command
//! printed. Memory stays at the limit plus one observed chunk.

use crate::limits::OutputLimit;

/// The last bytes and lines of a stream.
pub(super) struct Tail {
    bytes: Vec<u8>,
    limit: OutputLimit,
    cut: bool,
}

impl Tail {
    pub(super) fn new(limit: OutputLimit) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            cut: false,
        }
    }

    /// Append `chunk`, keeping only the tail. Only the last `max_bytes` of
    /// a chunk are ever copied, however large the chunk.
    pub(super) fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let max = usize::try_from(self.limit.max_bytes).unwrap_or(usize::MAX);
        let skip = chunk.len().saturating_sub(max);
        self.cut |= skip > 0;
        self.bytes.extend_from_slice(&chunk[skip..]);
        if self.bytes.len() > max {
            let drop = self.bytes.len() - max;
            self.bytes.drain(..drop);
            self.cut = true;
        }
        if let Some(max_lines) = self.limit.max_lines {
            self.keep_lines(usize::try_from(max_lines).unwrap_or(usize::MAX));
        }
    }

    /// Drop whole leading lines until at most `max` remain. A last line
    /// without a newline counts as a line.
    fn keep_lines(&mut self, max: usize) {
        let newlines = self.bytes.iter().filter(|&&b| b == b'\n').count();
        let open = usize::from(self.bytes.last().is_some_and(|&b| b != b'\n'));
        let lines = newlines + open;
        if lines <= max {
            return;
        }
        let mut excess = lines - max;
        let mut start = self.bytes.len();
        for (at, &byte) in self.bytes.iter().enumerate() {
            if byte == b'\n' {
                excess -= 1;
                if excess == 0 {
                    start = at + 1;
                    break;
                }
            }
        }
        self.bytes.drain(..start);
        self.cut = true;
    }

    /// The tail and whether earlier output was left out.
    pub(super) fn into_parts(self) -> (Vec<u8>, bool) {
        (self.bytes, self.cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_lines_and_bytes() {
        let limit = OutputLimit {
            max_bytes: 8,
            max_lines: Some(2),
        };
        let mut tail = Tail::new(limit);
        tail.push(b"a\nb\n");
        assert_eq!(tail.into_parts(), (b"a\nb\n".to_vec(), false));
        let mut tail = Tail::new(limit);
        tail.push(b"one\ntwo\nthree");
        assert_eq!(tail.into_parts(), (b"wo\nthree".to_vec(), true));
        let mut tail = Tail::new(limit);
        tail.push(b"a\nb\nc");
        assert_eq!(tail.into_parts(), (b"b\nc".to_vec(), true));
        let mut tail = Tail::new(limit);
        tail.push(b"1\n2\n");
        tail.push(b"3\n");
        assert_eq!(tail.into_parts(), (b"2\n3\n".to_vec(), true));
    }

    #[test]
    fn bytes_only_limit_cuts_mid_line() {
        let mut tail = Tail::new(OutputLimit {
            max_bytes: 3,
            max_lines: None,
        });
        tail.push(b"abcdef");
        assert_eq!(tail.into_parts(), (b"def".to_vec(), true));
        let mut tail = Tail::new(OutputLimit {
            max_bytes: 3,
            max_lines: None,
        });
        tail.push(b"ab");
        tail.push(b"cdefgh");
        assert_eq!(tail.into_parts(), (b"fgh".to_vec(), true));
    }
}
