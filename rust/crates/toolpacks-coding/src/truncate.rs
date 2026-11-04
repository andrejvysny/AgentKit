//! Head and tail truncation of tool output by lines and bytes. Mirrors
//! `packages/toolpacks-coding/src/truncate.ts` (Pi-derived, MIT).

/// Result of a bounded truncation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Truncation {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    /// Tail only: the first kept line is the end of a longer line.
    pub last_line_partial: bool,
    /// Head only: the very first line alone exceeds the byte limit.
    pub first_line_exceeds_limit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Clone, Copy, Debug)]
pub struct TruncationLimits {
    pub max_lines: usize,
    pub max_bytes: usize,
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    // JS `toFixed(1)` picks the LARGER digit on exact ties while `{:.1}`
    // rounds half-to-even. `bytes * 5 / 512` (KB) and `bytes * 5 / 524288`
    // (MB) are exact in f64, so floor(r + 0.5) reproduces toFixed's
    // nearest-with-ties-to-larger choice, and the digits are printed
    // exactly — no second rounding.
    if bytes < 1024 * 1024 {
        let tenths = (bytes as f64 * 5.0 / 512.0 + 0.5).floor() as u64;
        return format!("{}.{}KB", tenths / 10, tenths % 10);
    }
    let tenths = (bytes as f64 * 5.0 / 524_288.0 + 0.5).floor() as u64;
    format!("{}.{}MB", tenths / 10, tenths % 10)
}

/// JS `split("\n")` minus the trailing empty entry for a trailing newline.
pub fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return vec![];
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn untouched(content: &str, lines: &[&str], bytes: usize) -> Truncation {
    Truncation {
        content: content.to_owned(),
        truncated: false,
        truncated_by: None,
        total_lines: lines.len(),
        total_bytes: bytes,
        output_lines: lines.len(),
        output_bytes: bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Keep the first lines/bytes; never returns a partial line.
pub fn truncate_head(content: &str, limits: TruncationLimits) -> Truncation {
    let total_bytes = content.len();
    let lines = split_lines(content);
    if lines.len() <= limits.max_lines && total_bytes <= limits.max_bytes {
        return untouched(content, &lines, total_bytes);
    }
    if lines.first().map(|line| line.len()).unwrap_or(0) > limits.max_bytes {
        return Truncation {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines: lines.len(),
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
        };
    }
    let mut kept: Vec<&str> = vec![];
    let mut bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (i, line) in lines.iter().enumerate().take(limits.max_lines) {
        let line_bytes = line.len() + if i > 0 { 1 } else { 0 };
        if bytes + line_bytes > limits.max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        bytes += line_bytes;
    }
    let out = kept.join("\n");
    Truncation {
        output_bytes: out.len(),
        output_lines: kept.len(),
        content: out,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines: lines.len(),
        total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Keep the last lines/bytes (errors live at the end of command output).
pub fn truncate_tail(content: &str, limits: TruncationLimits) -> Truncation {
    let total_bytes = content.len();
    let lines = split_lines(content);
    if lines.len() <= limits.max_lines && total_bytes <= limits.max_bytes {
        return untouched(content, &lines, total_bytes);
    }
    let mut kept: Vec<String> = vec![];
    let mut bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    let mut index = lines.len();
    while index > 0 && kept.len() < limits.max_lines {
        index -= 1;
        let line = lines[index];
        let line_bytes = line.len() + if !kept.is_empty() { 1 } else { 0 };
        if bytes + line_bytes > limits.max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if kept.is_empty() {
                let cut = tail_str_bytes(line, limits.max_bytes);
                kept.insert(0, cut);
                last_line_partial = true;
            }
            break;
        }
        kept.insert(0, line.to_owned());
        bytes += line_bytes;
    }
    let out = kept.join("\n");
    Truncation {
        output_bytes: out.len(),
        output_lines: kept.len(),
        content: out,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines: lines.len(),
        total_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
    }
}

/// Last `max_bytes` of a string, starting on a UTF-8 character boundary.
pub fn tail_str_bytes(value: &str, max_bytes: usize) -> String {
    let bytes = value.as_bytes();
    if bytes.len() <= max_bytes {
        return value.to_owned();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
        start += 1;
    }
    value[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::format_size;

    #[test]
    fn format_size_matches_js_tofixed_including_exact_ties() {
        // Expectations recorded from the TypeScript side under bun:
        // (bytes/1024).toFixed(1) / (bytes/(1024*1024)).toFixed(1).
        assert_eq!(format_size(511), "511B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(1280), "1.3KB"); // 1.25 tie: JS picks 1.3
        assert_eq!(format_size(1792), "1.8KB"); // 1.75 tie: JS picks 1.8
        assert_eq!(format_size(6400), "6.3KB"); // 6.25 tie: JS picks 6.3
        assert_eq!(format_size(1_048_576), "1.0MB");
        assert_eq!(format_size(1_179_648), "1.1MB");
        assert_eq!(format_size(1_310_720), "1.3MB"); // 1.25 tie: JS picks 1.3
        assert_eq!(format_size(2_097_152), "2.0MB");
    }
}
