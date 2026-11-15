//! `run_tests`: a counts line, then everything a failure needs. Lines that
//! only say a test passed (and the runner's start-up and progress noise) are
//! dropped and counted. Recognised runners: cargo test, pytest, jest, vitest,
//! go test. Any other output keeps its text.

use super::{ShapeInput, Shaped, group, is_failure_line, lines, require_failures_kept};
use regex::Regex;
use std::sync::LazyLock;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static pattern")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Runner {
    Cargo,
    Pytest,
    Jest,
    Vitest,
    Go,
}

impl Runner {
    fn name(self) -> &'static str {
        match self {
            Self::Cargo => "cargo test",
            Self::Pytest => "pytest",
            Self::Jest => "jest",
            Self::Vitest => "vitest",
            Self::Go => "go test",
        }
    }
}

static CARGO_MARK: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^(test result: (ok|FAILED)\.|running \d+ tests?$)"));
static PYTEST_MARK: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^=+ (test session starts|.*\bin [\d.]+s.*) =+$"));
static VITEST_MARK: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*Test Files\s+\d+"));
static JEST_MARK: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^(Test Suites:|Tests:\s+.*\btotal\b)"));
static GO_MARK: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^(\s*--- (PASS|FAIL|SKIP): |=== RUN |ok\s+\S+\s+[\d.]+s|FAIL\s+\S+\s+[\d.]+s)")
});

static CARGO_DROP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(test \S+ \.\.\. (ok|ignored)(, .*)?|running \d+ tests?|\s*(Compiling|Finished|Running|Doc-tests|Fresh|Downloading|Downloaded|Blocking) .*)$",
    )
});
static PYTEST_DROP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^([\w./\\-]+\.py [.sxp]+\s*(\[\s*\d+%\])?|\S+::\S+ (PASSED|SKIPPED|XFAIL|XPASS)\s*(\[\s*\d+%\])?|=+ test session starts =+|platform .*|rootdir: .*|cachedir: .*|plugins?: .*|configfile: .*|collected \d+ items?.*|collecting \.\.\. .*)$",
    )
});
static JEST_DROP: LazyLock<Regex> = LazyLock::new(|| re(r"^(\s*(✓|√|✔)\s.*|PASS\s.*)$"));
static VITEST_DROP: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*(✓|√)\s.*$"));
static GO_DROP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(\s*=== (RUN|PAUSE|CONT|NAME)\b.*|\s*--- (PASS|SKIP): .*|PASS|ok\s+\S+\s+[\d.]+s.*|\?\s+\S+\s+\[no test files\])$",
    )
});

fn detect(text: &str) -> Option<Runner> {
    if VITEST_MARK.is_match(text) {
        Some(Runner::Vitest)
    } else if JEST_MARK.is_match(text) {
        Some(Runner::Jest)
    } else if PYTEST_MARK.is_match(text) {
        Some(Runner::Pytest)
    } else if CARGO_MARK.is_match(text) {
        Some(Runner::Cargo)
    } else if GO_MARK.is_match(text) {
        Some(Runner::Go)
    } else {
        None
    }
}

fn drop_pattern(runner: Runner) -> &'static Regex {
    match runner {
        Runner::Cargo => &*CARGO_DROP,
        Runner::Pytest => &*PYTEST_DROP,
        Runner::Jest => &*JEST_DROP,
        Runner::Vitest => &*VITEST_DROP,
        Runner::Go => &*GO_DROP,
    }
}

/// The dropped lines that say a test or suite passed; any other dropped line
/// is runner start-up or build chatter.
static CARGO_PASS: LazyLock<Regex> = LazyLock::new(|| re(r"^test \S+ \.\.\. (ok|ignored)(, .*)?$"));
static PYTEST_PASS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^([\w./\\-]+\.py [.sxp]+\s*(\[\s*\d+%\])?|\S+::\S+ (PASSED|SKIPPED|XFAIL|XPASS)\s*(\[\s*\d+%\])?)$",
    )
});
static GO_PASS: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*--- (PASS|SKIP): .*$"));

fn pass_pattern(runner: Runner) -> &'static Regex {
    match runner {
        Runner::Cargo => &*CARGO_PASS,
        Runner::Pytest => &*PYTEST_PASS,
        Runner::Jest => &*JEST_DROP,
        Runner::Vitest => &*VITEST_DROP,
        Runner::Go => &*GO_PASS,
    }
}

fn count_matching(lines: &[&str], pattern: &str) -> usize {
    let pattern = re(pattern);
    lines.iter().filter(|line| pattern.is_match(line)).count()
}

/// `runner: 124 passed, 2 failed, 3 ignored`, from the raw lines.
fn counts_line(runner: Runner, lines: &[&str]) -> String {
    let body = match runner {
        Runner::Cargo => format!(
            "{} passed, {} failed, {} ignored",
            count_matching(lines, r"^test \S+ \.\.\. ok"),
            count_matching(lines, r"^test \S+ \.\.\. FAILED"),
            count_matching(lines, r"^test \S+ \.\.\. ignored"),
        ),
        Runner::Pytest => {
            let summary = re(r"^=+ (.+? in [\d.]+s[^=]*?) =+$");
            lines
                .iter()
                .rev()
                .find_map(|line| summary.captures(line).map(|caps| caps[1].trim().to_owned()))
                .unwrap_or_else(|| "no summary line".to_owned())
        }
        Runner::Jest | Runner::Vitest => {
            let tests = re(r"^\s*Tests:?\s+(.*)$");
            lines
                .iter()
                .rev()
                .find_map(|line| tests.captures(line).map(|caps| caps[1].trim().to_owned()))
                .unwrap_or_else(|| "no summary line".to_owned())
        }
        Runner::Go => format!(
            "{} passed, {} failed, {} packages ok",
            count_matching(lines, r"^\s*--- PASS: "),
            count_matching(lines, r"^\s*--- FAIL: "),
            count_matching(lines, r"^ok\s+\S+\s+[\d.]+s"),
        ),
    };
    format!("{}: {body}", runner.name())
}

pub fn shape(input: &ShapeInput<'_>) -> Result<Option<Shaped>, String> {
    let Some(runner) = detect(input.text) else {
        return Ok(None);
    };
    let all = lines(input.text);
    let drop = drop_pattern(runner);
    let passing = pass_pattern(runner);
    let mut kept: Vec<&str> = Vec::new();
    let mut hidden = 0u64;
    let mut hidden_passing = 0u64;
    for &line in &all {
        // A line that names a failure stays, whatever else it looks like.
        if drop.is_match(line) && !is_failure_line(line) {
            hidden += 1;
            hidden_passing += u64::from(passing.is_match(line));
        } else if line.trim().is_empty() && kept.last().is_none_or(|last| last.trim().is_empty()) {
            // Blank runs left by dropped lines collapse.
        } else {
            kept.push(line);
        }
    }
    if hidden == 0 {
        return Ok(None);
    }
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    let text = format!("{}\n{}", counts_line(runner, &all), kept.join("\n"));
    require_failures_kept(input.text, &text)?;
    let what = format!(
        "lines ({} passing-test, {} build/progress)",
        group(hidden_passing),
        group(hidden - hidden_passing)
    );
    Ok(Some(Shaped::new(text, hidden, &what)))
}
