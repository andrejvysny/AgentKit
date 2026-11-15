//! `run_command`: progress and download lines collapse to a count in place;
//! error and warning blocks, program output and the exit-code footer stay.
//! A run of dropped lines becomes `[… N progress lines]` where it was.

use super::{ShapeInput, Shaped, group, is_failure_line, lines, require_failures_kept};
use regex::Regex;
use std::sync::LazyLock;

/// A line that names a failure is never progress, whatever else it looks like
/// (checked on the text without ANSI codes: the caller strips them).
static FAILURE_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)error|failed|failure|panic|fatal|exception|traceback").expect("static pattern")
});

/// Lines that only report progress: package-manager and build chatter,
/// download and layer bars. Bars are matched by their shape (a bar glyph run
/// or `N%|`), never by a bare percentage.
static PROGRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)^(
          \s*(Compiling|Downloading|Downloaded|Fresh|Checking|Updating|Locking|Adding|Blocking|Packaging|Documenting|Unpacking|Fetching|Resolving|Installing|Installed|Collecting|Building\ wheel)\s.*
        | \s*Progress:\ .*
        | \s*(Get|Hit|Ign):\d+\ .*
        | (Fetched|Reading\ package\ lists|Building\ dependency\ tree|Reading\ state\ information|Selecting\ previously|Preparing\ to\ unpack|Unpacking|Setting\ up)\b.*
        | \s*Requirement\ already\ satisfied:.*
        | \s*Using\ cached\ .*
        | \s*[0-9a-f]{12}:\ (Pulling\ fs\ layer|Waiting|Downloading|Verifying\ Checksum|Download\ complete|Extracting|Pull\ complete).*
        | \#\d+\ (sha256:[0-9a-f]+\ .*|[\d.]+\ ?[kKMG]?i?B\ /\ [\d.]+\ ?[kKMG]?i?B.*)
        | .*[█▉▊▋▌▍▎▏━]{8,}.*\d+(\.\d+)?\s?%.*
        | .*\[=+>?\s*\]\s*\d+(\.\d+)?\s?%.*
        | \s*\d+%\|.*
        | \s*\d+%\ ?[\[█▉▊▋▌▍▎▏━\#>=-]{3,}.*
        | npm\ (http|timing)\ .*
        )$",
    )
    .expect("static pattern")
});

pub fn shape(input: &ShapeInput<'_>) -> Result<Option<Shaped>, String> {
    let all = lines(input.text);
    let mut out: Vec<String> = Vec::with_capacity(all.len());
    let mut run = 0u64;
    let mut hidden = 0u64;
    let flush = |out: &mut Vec<String>, run: &mut u64| {
        if *run > 0 {
            let noun = if *run == 1 {
                "progress line"
            } else {
                "progress lines"
            };
            out.push(format!("[… {} {noun}]", group(*run)));
            *run = 0;
        }
    };
    for line in &all {
        if PROGRESS.is_match(line) && !FAILURE_WORD.is_match(line) && !is_failure_line(line) {
            run += 1;
            hidden += 1;
        } else {
            flush(&mut out, &mut run);
            out.push((*line).to_owned());
        }
    }
    flush(&mut out, &mut run);
    if hidden == 0 {
        return Ok(None);
    }
    let text = out.join("\n");
    require_failures_kept(input.text, &text)?;
    Ok(Some(Shaped::new(text, hidden, "progress lines")))
}
