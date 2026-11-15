//! `read_file`: a read with a range (`offset` or `limit`) is unchanged. A
//! whole-file read above the inline limit becomes an outline of the file's
//! symbols with line numbers, its first lines, and the handle that holds the
//! full text. Symbols come from one regex per language (rust, ts/js, python,
//! go); any other file keeps its text.

use super::{ShapeInput, Shaped, clip, group};
use regex::Regex;
use std::sync::LazyLock;

const FIRST_LINES: usize = 40;
const MAX_SYMBOLS: usize = 150;
const SYMBOL_CHARS: usize = 110;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Script,
    Python,
    Go,
}

fn language(path: &str) -> Option<Lang> {
    let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
    match extension.as_str() {
        "rs" => Some(Lang::Rust),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" => Some(Lang::Script),
        "py" => Some(Lang::Python),
        "go" => Some(Lang::Go),
        _ => None,
    }
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static pattern")
}

/// The tool's own cap notice: `[Showing lines 1-2000 of 5120 …]`.
static CAPPED: LazyLock<Regex> = LazyLock::new(|| re(r"^\[Showing lines (\d+)-(\d+) of (\d+)\b"));
static NUMBERED: LazyLock<Regex> = LazyLock::new(|| re(r"^(\d+)\t(.*)$"));
static RUST: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"^(\s*)((?:pub(?:\([^)]*\))?\s+)?(?:(?:async|unsafe|const|extern\s+"[^"]*")\s+)*(?:fn|struct|enum|trait|impl|mod|type|static|const|macro_rules!)\b.*)$"#,
    )
});
static SCRIPT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(\s*)((?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(?:function\*?\s+\w+|class\s+\w+|interface\s+\w+|type\s+\w+\s*[=<]|enum\s+\w+|(?:const|let)\s+\w+\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\([^)]*\)|\w+)\s*(?::[^=]+)?=>|(?:const|let)\s+\w+\s*=\s*(?:async\s+)?function).*)$",
    )
});
static SCRIPT_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(\s{2,4})((?:(?:public|private|protected|static|async|readonly|override|get|set)\s+)*\w+\s*(?:<[^>]*>)?\([^)]*\)\s*(?::\s*[^{;]+)?\s*\{.*)$",
    )
});
static PYTHON: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(\s*)((?:async\s+)?(?:def|class)\s+\w+.*)$"));
static GO: LazyLock<Regex> = LazyLock::new(|| re(r"^()((?:func|type|var|const)\b.*)$"));

const NOT_METHODS: [&str; 8] = [
    "if", "for", "while", "switch", "catch", "return", "function", "else",
];

fn symbol(lang: Lang, line: &str) -> Option<(usize, String)> {
    let caps = match lang {
        Lang::Rust => RUST.captures(line),
        Lang::Python => PYTHON.captures(line),
        Lang::Go => GO.captures(line),
        Lang::Script => SCRIPT.captures(line).or_else(|| {
            SCRIPT_METHOD.captures(line).filter(|caps| {
                let name = caps[2]
                    .split(|c: char| !c.is_alphanumeric())
                    .find(|w| !w.is_empty());
                name.is_some_and(|word| !NOT_METHODS.contains(&word))
            })
        }),
    }?;
    let indent = caps[1].len();
    if indent > 8 {
        return None;
    }
    let text = caps[2]
        .trim_end()
        .trim_end_matches(['{', ' ', ','])
        .trim_end();
    Some((indent / 2, clip(text, SYMBOL_CHARS)))
}

pub fn shape(input: &ShapeInput<'_>) -> Result<Option<Shaped>, String> {
    let Some(args) = input.args.as_object() else {
        return Ok(None);
    };
    if args.contains_key("offset") || args.contains_key("limit") || args.contains_key("paths") {
        return Ok(None);
    }
    if !input.ok || input.text.len() <= input.inline_limit {
        return Ok(None);
    }
    let Some(path) = args.get("file_path").and_then(|value| value.as_str()) else {
        return Ok(None);
    };
    let Some(lang) = language(path) else {
        return Ok(None);
    };
    let mut numbered = 0usize;
    let mut outline: Vec<String> = Vec::new();
    let mut first: Vec<&str> = Vec::new();
    let mut notices: Vec<&str> = Vec::new();
    for line in input.text.lines() {
        let Some(caps) = NUMBERED.captures(line) else {
            notices.push(line);
            continue;
        };
        numbered += 1;
        if numbered <= FIRST_LINES {
            first.push(line);
        }
        if outline.len() < MAX_SYMBOLS
            && let Some((depth, text)) = symbol(lang, &caps[2])
        {
            outline.push(format!(
                "{:>6}  {}{text}",
                &caps[1],
                "  ".repeat(depth.min(3))
            ));
        }
    }
    if outline.is_empty() || numbered <= FIRST_LINES {
        return Ok(None);
    }
    // When the tool capped the read, its notice carries the file's real
    // length; the lines past the cap are not in the stored text.
    let cap = notices
        .iter()
        .find_map(|line| CAPPED.captures(line))
        .and_then(|caps| {
            Some((
                caps[2].parse::<usize>().ok()?,
                caps[3].parse::<usize>().ok()?,
            ))
        });
    let total = cap.map_or(numbered, |(_, total)| total.max(numbered));
    let mut out = vec![format!(
        "{path}: {} lines. Outline of symbols below, then the first {FIRST_LINES} lines; \
         call read_file with offset and limit for a range.",
        group(total as u64)
    )];
    out.push("outline (line  symbol):".to_owned());
    out.extend(outline);
    out.push(format!("first {FIRST_LINES} lines:"));
    out.extend(first.iter().map(|line| (*line).to_owned()));
    out.extend(notices.iter().map(|line| (*line).to_owned()));
    let shaped = Shaped::new(out.join("\n"), (total - FIRST_LINES) as u64, "lines");
    Ok(Some(match cap {
        Some((last, total)) => shaped.with_note(format!(
            "the file has {} lines; the handle holds lines 1-{last} only",
            group(total as u64)
        )),
        None => shaped,
    }))
}
