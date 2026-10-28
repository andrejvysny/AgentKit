//! Bounded, ignore-aware filesystem walk and text search. Port of
//! `packages/toolpacks-coding/src/search.ts`.
//!
//! Adapted from OneMind (https://github.com/andrejvysny/OneMind), commit
//! 51abbbd, path `src-ts/src/code/pi-worker/search.ts`.
//! Copyright (c) 2026 OneMind contributors. MIT License.
//! Changes: no Bun.Glob (own glob compiler with braces), no Pi grep wrapper,
//! limits come from `CodingLimits`, directories can be yielded, depth bound,
//! injectable clock, `.git`/`node_modules`/symlinks/sensitive names skipped.
//!
//! # `regex`-crate divergence from JS `RegExp`
//!
//! TS compiles user patterns with `new RegExp(pattern, flags)` and reports
//! `invalid_pattern` when it throws. The Rust port uses the `regex` crate,
//! which implements RE2-style matching. Consequences:
//!
//! - Patterns JS rejects (unbalanced `(`, bad class ranges, dangling `\`) are
//!   rejected identically here: same `invalid_pattern` code and message.
//! - Patterns JS accepts but the `regex` crate lacks — lookaheads/lookbehinds
//!   `(?=…)`, `(?!…)`, `(?<=…)`, `(?<!…)`, backreferences `\1`, atomic groups,
//!   `\A`/`\z`, `(?<name>…)` references — are reported as `invalid_pattern`
//!   too. This is the only accepted-pattern divergence: Rust is strictly
//!   narrower for regex features while the error shape is unchanged.
//! - Glob compilation only emits constructs (`[…]`, `(?:…)`, `|`, `*`, `+`)
//!   both engines accept; malformed rules are dropped exactly as TS drops
//!   rules whose `new RegExp` throws.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use regex::{Regex, RegexBuilder};
use tokio_util::sync::CancellationToken;

use crate::errors::CodingError;
use crate::fs_util;
use crate::limits::CodingLimits;
use crate::paths::{is_sensitive_segment, relative};
use crate::types::Clock;

/// Git's exclude files plus the ripgrep-style `.ignore`.
const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".git/info/exclude"];

/// Why a walk ended early; reported to the model instead of failing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchBudget {
    pub entries_exhausted: bool,
    pub timed_out: bool,
}

#[derive(Clone)]
struct IgnoreRule {
    base: String,
    pattern: Regex,
    negate: bool,
    directory_only: bool,
}

/// One yielded entry of [`Walk`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkEntry {
    pub path: String,
    /// Relative to the walk root, with `/` separators.
    pub rel: String,
    pub is_directory: bool,
    pub depth: usize,
}

/// Shared walk configuration; mirrors TS `WalkOptions` (the mutable `budget`
/// lives inside [`Walk`], and `inherited_from` is set by the callers).
#[derive(Clone)]
pub struct WalkOptions {
    pub limits: CodingLimits,
    pub allow_sensitive: bool,
    pub clock: Clock,
    /// Replaces the TS `AbortSignal`; checked where TS checks `signal.aborted`.
    pub signal: Option<CancellationToken>,
    /// Workspace root: ignore files above the walk root still apply.
    pub inherited_from: Option<String>,
    pub include_directories: bool,
    /// Entries deeper than this are not listed (root children are depth 1).
    pub max_depth: Option<usize>,
}

impl WalkOptions {
    /// The `walkBase(env, signal)` equivalent used by the tool wrappers.
    pub fn new(
        limits: CodingLimits,
        allow_sensitive: bool,
        clock: Clock,
        signal: Option<CancellationToken>,
    ) -> Self {
        Self {
            limits,
            allow_sensitive,
            clock,
            signal,
            inherited_from: None,
            include_directories: false,
            max_depth: None,
        }
    }
}

fn aborted() -> CodingError {
    CodingError::new("aborted", "the call was aborted")
}

fn cancelled(signal: &Option<CancellationToken>) -> bool {
    signal.as_ref().is_some_and(CancellationToken::is_cancelled)
}

/// `path.join(dir, name)` for the POSIX separators the confined paths use.
fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// `path.basename` for walk-produced relative paths and direct file targets.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(at) => &trimmed[at + 1..],
        None => trimmed,
    }
}

/// JS `str.slice(0, n)` on chars, returned as a borrow when possible.
fn clip_chars(line: &str, n: usize) -> &str {
    match line.char_indices().nth(n) {
        Some((at, _)) => &line[..at],
        None => line,
    }
}

/// `JSON.stringify(value).slice(0, 200)` — chars, like JS.
fn json_snippet(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
    clip_chars(&quoted, 200).to_owned()
}

/// JS `ms / 1000` rendered the way JS renders the number.
fn seconds(ms: u64) -> String {
    let value = ms as f64 / 1000.0;
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Shallow-first walk in name order: a search that must stop early has covered
/// the top of the project. Never enters `.git`, `node_modules`, symlinks,
/// ignored paths or (unless allowed) sensitive names.
///
/// The TS `async function* walkTree` becomes a pull iterator: [`Walk::next`]
/// yields one entry per call and stops consulting the filesystem once a budget
/// flag trips; dropping the `Walk` abandons the walk exactly like `break`
/// ends the TS generator.
pub struct Walk {
    root: String,
    options: WalkOptions,
    cache: HashMap<String, Arc<Vec<IgnoreRule>>>,
    queue: VecDeque<(String, Arc<Vec<IgnoreRule>>, usize)>,
    current: Option<DirState>,
    deadline: i64,
    scanned: usize,
    budget: SearchBudget,
    done: bool,
}

#[derive(Clone)]
struct ListedEntry {
    name: String,
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

struct DirState {
    directory: String,
    rules: Arc<Vec<IgnoreRule>>,
    depth: usize,
    entries: Vec<ListedEntry>,
    cursor: usize,
}

impl Walk {
    /// Mirrors the TS generator start: computes the inherited ignore rules of
    /// the root, then yields entries via [`Walk::next`].
    pub async fn new(root: &str, options: &WalkOptions) -> Result<Self, CodingError> {
        let mut cache = HashMap::new();
        let root_rules = Arc::new(inherited_rules(root, options, &mut cache).await);
        Ok(Self {
            root: root.to_owned(),
            options: options.clone(),
            cache,
            queue: VecDeque::from([(root.to_owned(), root_rules, 0)]),
            current: None,
            deadline: (options.clock)() + options.limits.search_time_budget_ms as i64,
            scanned: 0,
            budget: SearchBudget::default(),
            done: false,
        })
    }

    /// Budget flags set so far; mirrors the shared `SearchBudget` object TS
    /// hands to the caller.
    pub fn budget(&self) -> &SearchBudget {
        &self.budget
    }

    /// Yields the next entry, or `None` once the walk is finished or bounded.
    /// Errs only with `aborted` when the token is cancelled.
    pub async fn next(&mut self) -> Result<Option<WalkEntry>, CodingError> {
        loop {
            if self.done {
                return Ok(None);
            }
            let mut state = match self.current.take() {
                Some(state) if state.cursor < state.entries.len() => state,
                _ => {
                    // Advance to the next queued directory, TS's while-loop head.
                    let Some((directory, rules, depth)) = self.queue.pop_front() else {
                        self.done = true;
                        return Ok(None);
                    };
                    if cancelled(&self.options.signal) {
                        self.current = None;
                        return Err(aborted());
                    }
                    if (self.options.clock)() > self.deadline {
                        self.budget.timed_out = true;
                        self.done = true;
                        return Ok(None);
                    }
                    let declared =
                        ignore_rules(&directory, &self.options.limits, &mut self.cache).await;
                    let effective = if declared.is_empty() {
                        rules
                    } else {
                        Arc::new(
                            rules
                                .iter()
                                .chain(declared.iter())
                                .cloned()
                                .collect::<Vec<_>>(),
                        )
                    };
                    let mut listed = match tokio::fs::read_dir(&directory).await {
                        Ok(mut reader) => {
                            let mut listed: Vec<ListedEntry> = Vec::new();
                            while let Ok(Some(entry)) = reader.next_entry().await {
                                let file_type = match entry.file_type().await {
                                    Ok(file_type) => file_type,
                                    Err(_) => continue,
                                };
                                listed.push(ListedEntry {
                                    name: entry.file_name().to_string_lossy().into_owned(),
                                    is_dir: file_type.is_dir(),
                                    is_file: file_type.is_file(),
                                    is_symlink: file_type.is_symlink(),
                                });
                            }
                            listed
                        }
                        Err(_) => {
                            // TS `catch { continue; }`
                            self.current = None;
                            continue;
                        }
                    };
                    listed.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
                    DirState {
                        directory,
                        rules: effective,
                        depth,
                        entries: listed,
                        cursor: 0,
                    }
                }
            };
            while state.cursor < state.entries.len() {
                let entry = state.entries[state.cursor].clone();
                state.cursor += 1;
                self.scanned += 1;
                if self.scanned > self.options.limits.search_max_files {
                    self.budget.entries_exhausted = true;
                    self.done = true;
                    self.current = None;
                    return Ok(None);
                }
                if entry.name == ".git"
                    || entry.name == "node_modules"
                    || entry.is_symlink
                    || (!self.options.allow_sensitive && is_sensitive_segment(&entry.name))
                {
                    continue;
                }
                let full = join(&state.directory, &entry.name);
                let is_directory = entry.is_dir;
                if excluded(&state.rules, &full, is_directory) {
                    continue;
                }
                let rel = relative(&self.root, &full);
                let depth = state.depth + 1;
                if is_directory {
                    if self.options.max_depth.is_none_or(|max| depth < max) {
                        self.queue
                            .push_back((full.clone(), Arc::clone(&state.rules), depth));
                    }
                    if self.options.include_directories {
                        self.current = Some(state);
                        return Ok(Some(WalkEntry {
                            path: full,
                            rel,
                            is_directory: true,
                            depth,
                        }));
                    }
                    continue;
                } else if entry.is_file {
                    self.current = Some(state);
                    return Ok(Some(WalkEntry {
                        path: full,
                        rel,
                        is_directory: false,
                        depth,
                    }));
                }
            }
            self.current = None;
        }
    }
}

/// Drains a [`Walk`] completely. The TS consumers break early; prefer driving
/// `Walk` directly when the caller can stop before the bounds trip.
pub async fn walk_tree(
    root: &str,
    options: &WalkOptions,
) -> Result<(Vec<WalkEntry>, SearchBudget), CodingError> {
    let mut walk = Walk::new(root, options).await?;
    let mut entries = Vec::new();
    while let Some(entry) = walk.next().await? {
        entries.push(entry);
    }
    Ok((entries, *walk.budget()))
}

async fn inherited_rules(
    root: &str,
    options: &WalkOptions,
    cache: &mut HashMap<String, Arc<Vec<IgnoreRule>>>,
) -> Vec<IgnoreRule> {
    let Some(from) = options.inherited_from.as_deref() else {
        return Vec::new();
    };
    let mut chain: Vec<String> = vec![from.to_owned()];
    let mut directory = from.to_owned();
    for part in relative(from, root).split('/') {
        if part.is_empty() || part == ".." {
            return Vec::new();
        }
        directory = join(&directory, part);
        chain.push(directory.clone());
    }
    // the walk root declares its own ignore file when reached
    chain.pop();
    let mut rules = Vec::new();
    for ancestor in chain {
        rules.extend(
            ignore_rules(&ancestor, &options.limits, cache)
                .await
                .iter()
                .cloned(),
        );
    }
    rules
}

async fn ignore_rules(
    directory: &str,
    limits: &CodingLimits,
    cache: &mut HashMap<String, Arc<Vec<IgnoreRule>>>,
) -> Arc<Vec<IgnoreRule>> {
    if let Some(cached) = cache.get(directory) {
        return Arc::clone(cached);
    }
    let mut rules: Vec<IgnoreRule> = Vec::new();
    for name in IGNORE_FILES {
        let path = join(directory, name);
        // Node `readFile(path, "utf8")` decodes lossily; a missing or
        // unreadable file is skipped like TS's catch.
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        // TS caps the decoded string length; byte length is the UTF-8
        // equivalent for ASCII ignore files, the only realistic case.
        if bytes.len() as u64 > limits.ignore_file_max_bytes {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for line in text.split('\n') {
            if let Some(rule) = parse_ignore_rule(line) {
                rules.push(IgnoreRule {
                    base: directory.to_owned(),
                    pattern: rule.0,
                    negate: rule.1,
                    directory_only: rule.2,
                });
            }
        }
    }
    let rules = Arc::new(rules);
    cache.insert(directory.to_owned(), Arc::clone(&rules));
    rules
}

/// The ignore decision [`Walk`] makes, for one directory's direct children:
/// rules from the workspace root down to `directory` itself.
pub struct IgnoreMatcher {
    directory: String,
    rules: Vec<IgnoreRule>,
}

impl IgnoreMatcher {
    /// TS returned a `(name, isDirectory) => boolean` callable.
    pub fn is_ignored(&self, name: &str, is_directory: bool) -> bool {
        excluded(&self.rules, &join(&self.directory, name), is_directory)
    }
}

pub async fn load_ignore_matcher(
    directory: &str,
    workspace_root: &str,
    limits: &CodingLimits,
) -> IgnoreMatcher {
    let mut cache = HashMap::new();
    let options = WalkOptions {
        limits: limits.clone(),
        inherited_from: Some(workspace_root.to_owned()),
        ..walk_stub_options(limits)
    };
    let mut rules = inherited_rules(directory, &options, &mut cache).await;
    rules.extend(
        ignore_rules(directory, limits, &mut cache)
            .await
            .iter()
            .cloned(),
    );
    IgnoreMatcher {
        directory: directory.to_owned(),
        rules,
    }
}

/// Placeholder base options for helper walks that only need limits and the
/// inherited root; the clock and signal are unused while compiling rules.
fn walk_stub_options(limits: &CodingLimits) -> WalkOptions {
    WalkOptions {
        limits: limits.clone(),
        allow_sensitive: false,
        clock: Arc::new(|| 0),
        signal: None,
        inherited_from: None,
        include_directories: false,
        max_depth: None,
    }
}

/// Parses one ignore-file line; `None` means blank/comment/malformed. The
/// tuple is `(pattern, negate, directory_only)`.
fn parse_ignore_rule(line: &str) -> Option<(Regex, bool, bool)> {
    // JS `trim()` strips U+FEFF (BOM) along with whitespace; match it so a
    // BOM-prefixed ignore file keeps its first rule.
    let mut text = line
        .strip_suffix('\r')
        .unwrap_or(line)
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
        .to_owned();
    if text.is_empty() || text.starts_with('#') {
        return None;
    }
    let mut negate = false;
    if let Some(rest) = text.strip_prefix('!') {
        negate = true;
        text = rest.to_owned();
    }
    let mut directory_only = false;
    if let Some(rest) = text.strip_suffix('/') {
        directory_only = true;
        text = rest.to_owned();
    }
    let anchored = text.contains('/');
    text = text.trim_start_matches('/').to_owned();
    if text.is_empty() {
        return None;
    }
    let body = glob_to_pattern(&text.chars().collect::<Vec<char>>());
    // a malformed ignore file must not break searching
    let pattern = RegexBuilder::new(&if anchored {
        format!("^{body}$")
    } else {
        format!("(?:^|/){body}$")
    })
    .build()
    .ok()?;
    Some((pattern, negate, directory_only))
}

fn glob_to_pattern(glob: &[char]) -> String {
    let mut pattern = String::new();
    let mut index = 0usize;
    while index < glob.len() {
        let char = glob[index];
        if char == '*' && glob.get(index + 1) == Some(&'*') {
            if glob.get(index + 2) == Some(&'/') {
                pattern.push_str("(?:[^/]*/)*");
                index += 3;
                continue;
            } else {
                pattern.push_str(".*");
                index += 2;
                continue;
            }
        } else if char == '*' {
            pattern.push_str("[^/]*");
        } else if char == '?' {
            pattern.push_str("[^/]");
        } else if char == '[' {
            match glob[index + 1..].iter().position(|c| *c == ']') {
                None => pattern.push_str("\\["),
                Some(offset) => {
                    let end = index + 1 + offset;
                    let inner = &glob[index + 1..end];
                    let body: String = if inner.first() == Some(&'!') {
                        std::iter::once('^')
                            .chain(inner[1..].iter().copied())
                            .collect()
                    } else {
                        inner.iter().collect()
                    };
                    pattern.push('[');
                    pattern.push_str(&body);
                    pattern.push(']');
                    index = end + 1;
                    continue;
                }
            }
        } else if char == '{' {
            match glob[index + 1..].iter().position(|c| *c == '}') {
                None => pattern.push_str("\\{"),
                Some(offset) => {
                    let end = index + 1 + offset;
                    let inner = &glob[index + 1..end];
                    let alternatives: Vec<String> = split_chars(inner, ',')
                        .iter()
                        .map(|part| glob_to_pattern(part))
                        .collect();
                    pattern.push_str("(?:");
                    pattern.push_str(&alternatives.join("|"));
                    pattern.push(')');
                    index = end + 1;
                    continue;
                }
            }
        } else if "\\^$.|?*+(){}".contains(char) {
            pattern.push('\\');
            pattern.push(char);
        } else {
            pattern.push(char);
        }
        index += 1;
    }
    pattern
}

/// `String.split(',')` on a `&[char]`, dropping nothing (JS keeps empties).
fn split_chars(parts: &[char], separator: char) -> Vec<Vec<char>> {
    let mut result: Vec<Vec<char>> = Vec::new();
    let mut current: Vec<char> = Vec::new();
    for char in parts {
        if *char == separator {
            result.push(std::mem::take(&mut current));
        } else {
            current.push(*char);
        }
    }
    result.push(current);
    result
}

/// Compile a user glob; errors with `CodingError` for an unusable one.
pub fn compile_glob(glob: &str) -> Result<Regex, CodingError> {
    match RegexBuilder::new(&format!(
        "^{}$",
        glob_to_pattern(&glob.chars().collect::<Vec<char>>())
    ))
    .build()
    {
        Ok(regex) => Ok(regex),
        Err(_) => Err(CodingError::new(
            "invalid_glob",
            format!(
                "invalid glob {}; use e.g. *.ts or **/*.test.tsx",
                json_snippet(glob)
            ),
        )),
    }
}

/// A glob matches the relative path, or just the file name for bare globs.
pub fn glob_matches(glob: &Regex, rel: &str) -> bool {
    glob.is_match(rel) || glob.is_match(basename(rel))
}

fn excluded(rules: &[IgnoreRule], file: &str, is_directory: bool) -> bool {
    let mut drop = false;
    for rule in rules {
        let path = relative(&rule.base, file);
        if path.is_empty() || path.starts_with("..") || (rule.directory_only && !is_directory) {
            continue;
        }
        if rule.pattern.is_match(&path) {
            drop = !rule.negate;
        }
    }
    drop
}

/// Input for [`search_text`]; mirrors TS `SearchInput`.
pub struct SearchInput {
    /// Absolute canonical directory or file to search.
    pub target: String,
    pub workspace_root: String,
    pub pattern: String,
    pub glob: Option<String>,
    pub literal: bool,
    pub ignore_case: bool,
    pub context: usize,
    pub limit: usize,
    /// Matches to skip, for paging.
    pub offset: usize,
}

/// Text, truncation flag and match count; mirrors TS `SearchOutcome`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOutcome {
    pub text: String,
    pub truncated: bool,
    pub matches: usize,
}

fn escape_pattern(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len());
    for char in pattern.chars() {
        if ".*+?^${}()|[]\\".contains(char) {
            escaped.push('\\');
        }
        escaped.push(char);
    }
    escaped
}

/// Bounded text search. Partial results come with a stated bound.
pub async fn search_text(
    input: &SearchInput,
    options: &WalkOptions,
) -> Result<SearchOutcome, CodingError> {
    let limits = &options.limits;
    if input.pattern.chars().count() > limits.search_max_pattern_chars {
        return Err(CodingError::new("search_limit", "pattern is too long"));
    }
    let regex = RegexBuilder::new(&if input.literal {
        escape_pattern(&input.pattern)
    } else {
        input.pattern.clone()
    })
    .case_insensitive(input.ignore_case)
    .build()
    .map_err(|_| {
        CodingError::new(
            "invalid_pattern",
            format!(
                "invalid regular expression {}; pass literal: true to search for the text itself",
                json_snippet(&input.pattern)
            ),
        )
    })?;
    let glob = input.glob.as_deref().map(compile_glob).transpose()?;
    let kind = match tokio::fs::metadata(&input.target).await {
        Ok(kind) => kind,
        Err(_) => {
            return Err(CodingError::new(
                "not_found",
                "nothing to search: path does not exist",
            ));
        }
    };
    let is_dir = kind.is_dir();
    let mut budget = SearchBudget::default();
    let output: Vec<String> = Vec::new();
    let mut state = SearchState {
        output,
        bytes: 0,
        output_bytes: 0,
        matches: 0,
        skipped: 0,
        input_exhausted: false,
        output_full: false,
    };
    let deadline = (options.clock)() + limits.search_time_budget_ms as i64;
    if is_dir {
        let walk_options = WalkOptions {
            inherited_from: Some(input.workspace_root.clone()),
            include_directories: false,
            max_depth: None,
            ..options.clone()
        };
        let mut walk = Walk::new(&input.target, &walk_options).await?;
        while let Some(file) = walk.next().await? {
            if cancelled(&options.signal) {
                return Err(aborted());
            }
            if (options.clock)() > deadline {
                budget.timed_out = true;
                break;
            }
            if let Some(glob) = &glob
                && !glob_matches(glob, &file.rel)
            {
                continue;
            }
            state
                .scan_file(
                    &file.path,
                    &file.rel,
                    input,
                    &regex,
                    limits,
                    options,
                    deadline,
                    &mut budget,
                )
                .await?;
            if state.matches >= input.limit
                || state.output_full
                || state.input_exhausted
                || walk.budget().timed_out
            {
                break;
            }
        }
        budget.entries_exhausted |= walk.budget().entries_exhausted;
        budget.timed_out |= walk.budget().timed_out;
    } else {
        let rel = basename(&input.target).to_owned();
        // The shared file-loop head in search.ts also governs the
        // single-file target: abort, deadline, and the glob filter all
        // apply before the file is read.
        if cancelled(&options.signal) {
            return Err(aborted());
        }
        if (options.clock)() > deadline {
            budget.timed_out = true;
        } else if let Some(glob) = &glob {
            if glob_matches(glob, &rel) {
                state
                    .scan_file(
                        &input.target,
                        &rel,
                        input,
                        &regex,
                        limits,
                        options,
                        deadline,
                        &mut budget,
                    )
                    .await?;
            }
        } else {
            state
                .scan_file(
                    &input.target,
                    &rel,
                    input,
                    &regex,
                    limits,
                    options,
                    deadline,
                    &mut budget,
                )
                .await?;
        }
    }
    let reason = if state.input_exhausted {
        // Math.round(bytes / 1024 / 1024)
        format!(
            "{} MiB of input scanned",
            (state.bytes as f64 / 1_048_576.0).round() as i64
        )
    } else if budget.entries_exhausted {
        format!("{} entries scanned", limits.search_max_files)
    } else if budget.timed_out {
        format!(
            "{}s of scanning elapsed",
            seconds(limits.search_time_budget_ms)
        )
    } else if state.matches >= input.limit {
        format!("match limit {} reached", input.limit)
    } else if state.output_full {
        "output limit reached".to_owned()
    } else {
        String::new()
    };
    let stopped = if reason.is_empty() {
        String::new()
    } else {
        format!(
            "{}; {} match{} so far, narrow the path, glob or pattern",
            reason,
            state.matches,
            if state.matches == 1 { "" } else { "es" }
        )
    };
    let body = clip_chars(&state.output.join("\n"), limits.search_max_output_bytes).to_owned();
    let more = if (state.matches >= input.limit || state.output_full) && !state.input_exhausted {
        format!(
            "\n[more matches: call again with offset={}]",
            input.offset + state.matches
        )
    } else {
        String::new()
    };
    let text = if !state.output.is_empty() {
        let stopped_part = if stopped.is_empty() {
            String::new()
        } else {
            format!("\n[stopped: {stopped}]")
        };
        format!("{body}{stopped_part}{more}")
    } else if !stopped.is_empty() {
        format!("No matches found ({stopped})")
    } else {
        "No matches found".to_owned()
    };
    Ok(SearchOutcome {
        text,
        truncated: !stopped.is_empty(),
        matches: state.matches,
    })
}

struct SearchState {
    output: Vec<String>,
    bytes: u64,
    output_bytes: usize,
    matches: usize,
    skipped: usize,
    input_exhausted: bool,
    output_full: bool,
}

impl SearchState {
    /// One file of [`search_text`]'s loop; only `aborted` can error out.
    #[allow(clippy::too_many_arguments)] // immutable loop context in one call
    async fn scan_file(
        &mut self,
        path: &str,
        rel: &str,
        input: &SearchInput,
        regex: &Regex,
        limits: &CodingLimits,
        options: &WalkOptions,
        deadline: i64,
        budget: &mut SearchBudget,
    ) -> Result<(), CodingError> {
        let info = match tokio::fs::metadata(path).await {
            Ok(info) => info,
            Err(_) => return Ok(()),
        };
        if !info.is_file() || info.len() > limits.search_max_file_bytes {
            return Ok(());
        }
        self.bytes += info.len();
        if self.bytes > limits.search_max_total_bytes {
            self.input_exhausted = true;
            return Ok(());
        }
        let raw = match tokio::fs::read(path).await {
            Ok(raw) => raw,
            Err(_) => return Ok(()),
        };
        if raw.contains(&0) {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&raw);
        let lines: Vec<&str> = text.split('\n').collect();
        for index in 0..lines.len() {
            // A huge file must not outlive a cancel or the time budget.
            if index % 256 == 255 {
                if cancelled(&options.signal) {
                    return Err(aborted());
                }
                if (options.clock)() > deadline {
                    budget.timed_out = true;
                    break;
                }
            }
            if !regex.is_match(clip_chars(lines[index], 8192)) {
                continue;
            }
            if self.skipped < input.offset {
                self.skipped += 1;
                continue;
            }
            let from = index.saturating_sub(input.context);
            let to = std::cmp::min(lines.len() - 1, index + input.context);
            let mut entries: Vec<String> = Vec::new();
            let mut size = 0usize;
            for (offset, text) in lines[from..=to].iter().enumerate() {
                let entry = format!(
                    "{rel}:{}:{}",
                    from + offset + 1,
                    clip_chars(text, limits.search_max_line_chars)
                );
                size += entry.len() + 1;
                entries.push(entry);
            }
            // A match is shown whole or left for the next page, never cut.
            if self.matches > 0 && self.output_bytes + size > limits.search_max_output_bytes {
                self.output_full = true;
                break;
            }
            self.output.extend(entries);
            self.output_bytes += size;
            self.matches += 1;
            if self.output_bytes >= limits.search_max_output_bytes {
                self.output_full = true;
            }
            if self.matches >= input.limit || self.output_full {
                break;
            }
        }
        Ok(())
    }
}

/// Paths plus the stop notice; mirrors TS `FindOutcome`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindOutcome {
    pub paths: Vec<String>,
    pub stopped: String,
}

/// Glob over the ignore-aware walk.
pub async fn find_files(
    pattern: &str,
    target: &str,
    workspace_root: &str,
    limit: usize,
    options: &WalkOptions,
) -> Result<FindOutcome, CodingError> {
    if pattern.chars().count() > options.limits.search_max_pattern_chars {
        return Err(CodingError::new("search_limit", "pattern is too long"));
    }
    let glob = compile_glob(pattern)?;
    let walk_options = WalkOptions {
        inherited_from: Some(workspace_root.to_owned()),
        include_directories: false,
        max_depth: None,
        ..options.clone()
    };
    let mut walk = Walk::new(target, &walk_options).await?;
    let mut paths: Vec<String> = Vec::new();
    let mut hit_limit = false;
    while let Some(entry) = walk.next().await? {
        if !glob_matches(&glob, &entry.rel) {
            continue;
        }
        paths.push(relative(workspace_root, &entry.path));
        if paths.len() >= limit {
            hit_limit = true;
            break;
        }
    }
    let budget = *walk.budget();
    let reasons: Vec<String> = [
        hit_limit.then(|| format!("result limit {limit} reached")),
        budget
            .entries_exhausted
            .then(|| format!("{} entries scanned", options.limits.search_max_files)),
        budget.timed_out.then(|| {
            format!(
                "{}s of scanning elapsed",
                seconds(options.limits.search_time_budget_ms)
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    Ok(FindOutcome {
        paths,
        stopped: reasons.join("; "),
    })
}

/// Manifests repo_map shows, lower-cased for the case-insensitive probe.
pub const MANIFESTS: [&str; 7] = [
    "package.json",
    "tsconfig.json",
    "cargo.toml",
    "pyproject.toml",
    "go.mod",
    "readme.md",
    "readme",
];

/// Rendered sections plus truncation state; the tool wrapper adds the summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMapOutcome {
    pub text: String,
    pub truncated: bool,
    pub entries: usize,
}

/// Top-level manifests with excerpts plus a depth-limited tree. Port of the
/// engine part of `repoMapTool` in `search-tools.ts`.
pub async fn repo_map(
    target: &str,
    workspace_root: &str,
    depth: usize,
    options: &WalkOptions,
) -> Result<RepoMapOutcome, CodingError> {
    let limits = &options.limits;
    let mut sections: Vec<String> = Vec::new();
    // Match against the real directory entries: a case-insensitive
    // filesystem would otherwise show README.md and readme.md twice.
    let mut present: Vec<String> = match tokio::fs::read_dir(target).await {
        Ok(mut reader) => {
            let mut names = Vec::new();
            while let Ok(Some(entry)) = reader.next_entry().await {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
            names
        }
        Err(_) => Vec::new(),
    };
    present.sort();
    for wanted in MANIFESTS {
        let Some(name) = present
            .iter()
            .find(|entry| entry.to_lowercase() == wanted)
            .cloned()
        else {
            continue;
        };
        if let Some(text) = read_excerpt(
            &join(target, &name),
            &name,
            limits.repo_map_manifest_excerpt_bytes,
        )
        .await
        {
            sections.push(format!("== {name} ==\n{text}"));
        }
    }
    let walk_options = WalkOptions {
        inherited_from: Some(workspace_root.to_owned()),
        include_directories: true,
        max_depth: Some(depth),
        ..options.clone()
    };
    let mut walk = Walk::new(target, &walk_options).await?;
    let mut entries: Vec<(String, bool)> = Vec::new();
    let mut more = false;
    while let Some(entry) = walk.next().await? {
        if entries.len() >= limits.repo_map_max_entries {
            more = true;
            break;
        }
        entries.push((entry.rel, entry.is_directory));
    }
    let budget = *walk.budget();
    entries.sort_by(|a, b| {
        let left: Vec<&str> = a.0.split('/').collect();
        let right: Vec<&str> = b.0.split('/').collect();
        compare_segments(&left, &right)
    });
    let tree: Vec<String> = entries
        .iter()
        .map(|(rel, is_directory)| {
            let parts: Vec<&str> = rel.split('/').collect();
            format!(
                "{}{}{}",
                "  ".repeat(parts.len() - 1),
                parts[parts.len() - 1],
                if *is_directory { "/" } else { "" }
            )
        })
        .collect();
    let notes: Vec<String> = [
        more.then(|| {
            format!(
                "{} entries shown; raise depth or narrow path",
                limits.repo_map_max_entries
            )
        }),
        budget
            .entries_exhausted
            .then(|| format!("{} entries scanned", limits.search_max_files)),
        budget.timed_out.then(|| {
            format!(
                "{}s of scanning elapsed",
                seconds(limits.search_time_budget_ms)
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    sections.push(format!(
        "== tree (depth {depth}) ==\n{}",
        if tree.is_empty() {
            "(empty)".to_owned()
        } else {
            tree.join("\n")
        }
    ));
    if !notes.is_empty() {
        sections.push(format!("[stopped: {}]", notes.join("; ")));
    }
    Ok(RepoMapOutcome {
        text: sections.join("\n\n"),
        truncated: !notes.is_empty(),
        entries: entries.len(),
    })
}

fn compare_segments(a: &[&str], b: &[&str]) -> std::cmp::Ordering {
    let n = std::cmp::min(a.len(), b.len());
    for index in 0..n {
        if a[index] != b[index] {
            return if a[index] < b[index] {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
    }
    a.len().cmp(&b.len())
}

async fn read_excerpt(path: &str, label: &str, max_bytes: usize) -> Option<String> {
    let buffer = fs_util::read_regular_file(path, label, 1024 * 1024)
        .await
        .ok()?;
    if buffer.iter().take(4096).any(|byte| *byte == 0) {
        return None;
    }
    let head = &buffer[..std::cmp::min(buffer.len(), max_bytes)];
    let text = String::from_utf8_lossy(head).into_owned();
    if buffer.len() > max_bytes {
        Some(format!(
            "{text}\n[... {} more bytes]",
            buffer.len() - max_bytes
        ))
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::system_clock;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicI64, Ordering};

    struct TempRoot {
        root: PathBuf,
    }

    impl TempRoot {
        async fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("ak-search-{tag}-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let root = tokio::fs::canonicalize(&root).await.unwrap();
            Self { root }
        }

        async fn put(&self, rel: &str, content: &str) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.unwrap();
            }
            tokio::fs::write(&path, content).await.unwrap();
        }

        fn path(&self, rel: &str) -> String {
            self.root.join(rel).to_string_lossy().into_owned()
        }

        fn root(&self) -> String {
            self.root.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn walk_options(limits: CodingLimits) -> WalkOptions {
        WalkOptions::new(limits, false, system_clock(), None)
    }

    fn input(root: &str, target: &str, pattern: &str) -> SearchInput {
        SearchInput {
            target: target.to_owned(),
            workspace_root: root.to_owned(),
            pattern: pattern.to_owned(),
            glob: None,
            literal: false,
            ignore_case: false,
            context: 0,
            limit: 1000,
            offset: 0,
        }
    }

    #[tokio::test]
    async fn finds_matches_with_path_line_text_and_honours_gitignore() {
        let h = TempRoot::new("gitignore").await;
        h.put(".gitignore", "ignored/\n*.log\n!keep.log\n").await;
        h.put("src/a.ts", "const needle = 1;\nother\n").await;
        h.put("ignored/b.ts", "needle in ignored dir\n").await;
        h.put("x.log", "needle in log\n").await;
        h.put("keep.log", "needle in kept log\n").await;
        let root = h.root();
        let outcome = search_text(
            &input(&root, &root, "needle"),
            &walk_options(CodingLimits::default()),
        )
        .await
        .unwrap();
        assert!(outcome.text.contains("src/a.ts:1:const needle = 1;"));
        assert!(outcome.text.contains("keep.log:1:needle in kept log"));
        assert!(!outcome.text.contains("ignored/b.ts"));
        assert!(!outcome.text.contains("x.log"));
    }

    #[tokio::test]
    async fn honours_ignore_file_and_skips_git_modules_symlinks_binary() {
        let h = TempRoot::new("skips").await;
        let outside = TempRoot::new("outside").await;
        h.put(".ignore", "scratch.txt\n").await;
        h.put("scratch.txt", "needle\n").await;
        h.put(".git/HEAD", "needle\n").await;
        h.put("node_modules/p/i.js", "needle\n").await;
        h.put("bin.dat", "needle\0\n").await;
        outside.put("o.txt", "needle\n").await;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path("o.txt"), h.path("link.txt")).unwrap();
            std::os::unix::fs::symlink(outside.root(), h.path("linkdir")).unwrap();
        }
        h.put("real.txt", "needle\n").await;
        let root = h.root();
        let outcome = search_text(
            &input(&root, &root, "needle"),
            &walk_options(CodingLimits::default()),
        )
        .await
        .unwrap();
        assert_eq!(outcome.text, "real.txt:1:needle");
        assert!(!outcome.truncated);
    }

    #[tokio::test]
    async fn sensitive_files_are_skipped() {
        let h = TempRoot::new("sensitive").await;
        h.put(".env", "API=needle\n").await;
        h.put("id_rsa", "needle\n").await;
        h.put("deploy.pem", "needle\n").await;
        h.put(".env.example", "API=needle-example\n").await;
        let root = h.root();
        let outcome = search_text(
            &input(&root, &root, "needle"),
            &walk_options(CodingLimits::default()),
        )
        .await
        .unwrap();
        assert_eq!(outcome.text, ".env.example:1:API=needle-example");
    }

    #[tokio::test]
    async fn direct_file_target_uses_basename() {
        let h = TempRoot::new("direct").await;
        h.put("f.txt", "needle here\n").await;
        let root = h.root();
        let outcome = search_text(
            &input(&root, &h.path("f.txt"), "needle"),
            &walk_options(CodingLimits::default()),
        )
        .await
        .unwrap();
        assert_eq!(outcome.text, "f.txt:1:needle here");
    }

    #[tokio::test]
    async fn literal_ignore_case_glob_context_and_line_clip() {
        let h = TempRoot::new("literal").await;
        h.put("a.ts", "l1\nl2\nHello (World)\nl4\nl5\n").await;
        h.put("b.md", "hello (world)\n").await;
        let root = h.root();
        let limits = CodingLimits::default();
        let mut search = input(&root, &root, "hello (world)");
        search.literal = true;
        search.ignore_case = true;
        search.glob = Some("*.ts".to_owned());
        search.context = 1;
        let outcome = search_text(&search, &walk_options(limits.clone()))
            .await
            .unwrap();
        assert_eq!(outcome.text, "a.ts:2:l2\na.ts:3:Hello (World)\na.ts:4:l4");

        let bad = input(&root, &h.path("a.ts"), "(");
        let error = search_text(&bad, &walk_options(limits.clone()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_pattern");
        assert_eq!(
            error.message,
            "invalid regular expression \"(\"; pass literal: true to search for the text itself"
        );

        h.put("long.txt", &format!("x{}\n", "y".repeat(3000))).await;
        let cut = input(&root, &h.path("long.txt"), "^x");
        let outcome = search_text(&cut, &walk_options(limits)).await.unwrap();
        assert_eq!(
            outcome.text.chars().count(),
            "long.txt:1:".chars().count() + 1024
        );
    }

    #[tokio::test]
    async fn match_limit_stop_notice() {
        let h = TempRoot::new("limit").await;
        h.put("m.txt", &["hit"; 10].join("\n")).await;
        let root = h.root();
        let mut search = input(&root, &root, "hit");
        search.limit = 3;
        let outcome = search_text(&search, &walk_options(CodingLimits::default()))
            .await
            .unwrap();
        assert!(outcome.truncated);
        assert!(outcome.text.contains(
            "[stopped: match limit 3 reached; 3 matches so far, narrow the path, glob or pattern]"
        ));
        assert!(
            outcome
                .text
                .contains("[more matches: call again with offset=3]")
        );
    }

    #[tokio::test]
    async fn entries_scanned_stop_notice() {
        let h = TempRoot::new("entries").await;
        let limits = CodingLimits {
            search_max_files: 5,
            ..CodingLimits::default()
        };
        for index in 0..20 {
            h.put(&format!("f{index:02}.txt"), "zzz\n").await;
        }
        let root = h.root();
        let outcome = search_text(&input(&root, &root, "nomatch"), &walk_options(limits))
            .await
            .unwrap();
        assert!(
            outcome
                .text
                .starts_with("No matches found (5 entries scanned;")
        );
        assert!(outcome.truncated);
    }

    #[tokio::test]
    async fn time_budget_stops_the_walk_and_says_so() {
        let h = TempRoot::new("budget").await;
        let limits = CodingLimits {
            search_time_budget_ms: 10,
            ..CodingLimits::default()
        };
        let clock_counter = Arc::new(AtomicI64::new(0));
        let clock: Clock = {
            let counter = Arc::clone(&clock_counter);
            Arc::new(move || counter.fetch_add(6, Ordering::SeqCst))
        };
        for index in 0..10 {
            h.put(&format!("d{index}/f.txt"), "zzz\n").await;
        }
        let root = h.root();
        let options = WalkOptions::new(limits, false, clock, None);
        let outcome = search_text(&input(&root, &root, "zzz"), &options)
            .await
            .unwrap();
        assert!(outcome.text.contains("s of scanning elapsed"));
        assert!(outcome.truncated);
    }

    #[tokio::test]
    async fn aborted_search_fails_with_aborted() {
        let h = TempRoot::new("abort").await;
        h.put("a.txt", "x").await;
        let token = CancellationToken::new();
        token.cancel();
        let root = h.root();
        let options = WalkOptions::new(CodingLimits::default(), false, system_clock(), Some(token));
        let error = search_text(&input(&root, &root, "x"), &options)
            .await
            .unwrap_err();
        assert_eq!(error.code, "aborted");
        assert_eq!(error.message, "the call was aborted");
    }

    #[tokio::test]
    async fn a_single_file_target_is_glob_filtered_too() {
        // search.ts:368 applies the glob inside the shared file loop, so a
        // single-file target that fails the glob yields no matches at all.
        let h = TempRoot::new("file-glob").await;
        h.put("util.js", "TODO fix\n").await;
        let root = h.root();
        let file = h.path("util.js");
        let mut search = input(&root, &file, "TODO");
        search.glob = Some("*.ts".to_owned());
        let outcome = search_text(&search, &walk_options(CodingLimits::default()))
            .await
            .unwrap();
        assert_eq!(outcome.matches, 0);
        assert!(!outcome.text.contains("TODO"));
        search.glob = Some("*.js".to_owned());
        let outcome = search_text(&search, &walk_options(CodingLimits::default()))
            .await
            .unwrap();
        assert!(outcome.text.contains("util.js:1:TODO fix"));
    }

    #[tokio::test]
    async fn a_single_file_target_respects_cancellation() {
        // The loop-head abort check in search.ts also runs for the synthetic
        // one-file iterable, so a pre-cancelled call must not read the file.
        let h = TempRoot::new("file-abort").await;
        h.put("a.txt", "needle\n").await;
        let token = CancellationToken::new();
        token.cancel();
        let root = h.root();
        let file = h.path("a.txt");
        let options = WalkOptions::new(CodingLimits::default(), false, system_clock(), Some(token));
        let error = search_text(&input(&root, &file, "needle"), &options)
            .await
            .unwrap_err();
        assert_eq!(error.code, "aborted");
    }

    #[tokio::test]
    async fn total_bytes_exhaustion_stops_the_walk_immediately() {
        // search.ts breaks the file loop when the running size crosses
        // searchMaxTotalBytes; the walk must not keep stat-ing (and
        // counting) later files, so the reported figure is the size at the
        // crossing point, not the whole tree.
        let h = TempRoot::new("total-bytes").await;
        let pad = "y".repeat(1024 * 1024);
        for name in [
            "a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt", "g.txt", "h.txt", "i.txt",
            "j.txt", "k.txt", "l.txt", "m.txt",
        ] {
            h.put(name, &pad).await;
        }
        h.put("n.txt", "needle at the end\n").await;
        let limits = CodingLimits {
            search_max_file_bytes: 4 * 1024 * 1024,
            search_max_total_bytes: 5 * 1024 * 1024,
            ..CodingLimits::default()
        };
        let root = h.root();
        let outcome = search_text(&input(&root, &root, "needle"), &walk_options(limits))
            .await
            .unwrap();
        assert!(!outcome.text.contains("n.txt"));
        assert_eq!(outcome.matches, 0);
        assert!(outcome.truncated);
        // a..d scanned (4 MiB), e crosses the 5 MiB cap: the walk stops
        // there, so later files are never counted. Without the loop break
        // the figure would keep inflating across all 14 files.
        assert!(
            outcome.text.contains("6 MiB of input scanned"),
            "got: {}",
            outcome.text
        );
    }

    #[tokio::test]
    async fn a_bom_prefixed_ignore_file_keeps_its_first_rule() {
        // JS trim() strips the BOM, so the first rule stays active.
        let h = TempRoot::new("bom").await;
        h.put(".gitignore", "\u{feff}vendor/\n").await;
        h.put("vendor/v.ts", "needle\n").await;
        h.put("src/a.ts", "needle\n").await;
        let root = h.root();
        let outcome = search_text(
            &input(&root, &root, "needle"),
            &walk_options(CodingLimits::default()),
        )
        .await
        .unwrap();
        assert!(outcome.text.contains("src/a.ts"));
        assert!(!outcome.text.contains("vendor/v.ts"));
    }

    #[tokio::test]
    async fn find_files_globs_braces_gitignore_sensitive_and_limit() {
        let h = TempRoot::new("find").await;
        h.put(".gitignore", "dist/\n").await;
        h.put("src/a.ts", "").await;
        h.put("src/deep/b.tsx", "").await;
        h.put("src/c.md", "").await;
        h.put("dist/d.ts", "").await;
        h.put(".env", "").await;
        h.put("keys.pem", "").await;
        let root = h.root();
        let options = walk_options(CodingLimits::default());

        let found = find_files("**/*.ts", &root, &root, 200, &options)
            .await
            .unwrap();
        assert_eq!(found.paths, vec!["src/a.ts".to_owned()]);

        let found = find_files("src/**/*.{ts,tsx}", &root, &root, 200, &options)
            .await
            .unwrap();
        assert_eq!(
            found.paths,
            vec!["src/a.ts".to_owned(), "src/deep/b.tsx".to_owned()]
        );

        let found = find_files("*.md", &h.path("src"), &root, 200, &options)
            .await
            .unwrap();
        assert_eq!(found.paths, vec!["src/c.md".to_owned()]);

        let found = find_files("**/.env*", &root, &root, 200, &options)
            .await
            .unwrap();
        assert!(found.paths.is_empty());

        let found = find_files("*.pem", &root, &root, 200, &options)
            .await
            .unwrap();
        assert!(found.paths.is_empty());

        let found = find_files("**/*", &root, &root, 1, &options).await.unwrap();
        assert_eq!(found.paths.len(), 1);
        assert_eq!(found.stopped, "result limit 1 reached");
    }

    #[tokio::test]
    async fn find_files_and_glob_failures() {
        let h = TempRoot::new("findbad").await;
        let root = h.root();
        let options = walk_options(CodingLimits::default());
        let error = find_files(&"x".repeat(5000), &root, &root, 200, &options)
            .await
            .unwrap_err();
        assert_eq!(error.code, "search_limit");
        assert_eq!(error.message, "pattern is too long");

        let limits = CodingLimits {
            search_max_pattern_chars: 10,
            ..CodingLimits::default()
        };
        let error = find_files("01234567890", &root, &root, 200, &walk_options(limits))
            .await
            .unwrap_err();
        assert_eq!(error.code, "search_limit");

        let error = compile_glob("[z-a]").unwrap_err();
        assert_eq!(error.code, "invalid_glob");
        assert_eq!(
            error.message,
            "invalid glob \"[z-a]\"; use e.g. *.ts or **/*.test.tsx"
        );
    }

    #[tokio::test]
    async fn repo_map_shows_manifests_and_a_depth_limited_tree() {
        let h = TempRoot::new("map").await;
        h.put("package.json", "{\"name\":\"demo\"}").await;
        h.put("README.md", "# Demo\n").await;
        h.put("src/a/b/c/d.ts", "").await;
        h.put("src/index.ts", "").await;
        h.put(".gitignore", "build/\n").await;
        h.put("build/out.js", "").await;
        h.put(".env", "A=1").await;
        let root = h.root();
        let options = walk_options(CodingLimits::default());
        let map = repo_map(&root, &root, 3, &options).await.unwrap();
        assert!(map.text.contains("== package.json ==\n{\"name\":\"demo\"}"));
        assert!(map.text.contains("== README.md ==\n# Demo"));
        assert!(map.text.contains("== tree (depth 3) =="));
        assert!(map.text.contains("src/\n  a/\n    b/\n  index.ts"));
        assert!(!map.text.contains("\n      c/"));
        assert!(!map.text.contains("build"));
        assert!(!map.text.contains(".env"));
        assert!(!map.truncated);

        let deep = repo_map(&root, &root, 5, &options).await.unwrap();
        assert!(deep.text.contains("d.ts"));
    }

    #[tokio::test]
    async fn repo_map_caps_tree_entries_and_says_so() {
        let h = TempRoot::new("mapcap").await;
        let limits = CodingLimits {
            repo_map_max_entries: 4,
            ..CodingLimits::default()
        };
        for index in 0..10 {
            h.put(&format!("f{index}.txt"), "").await;
        }
        let root = h.root();
        let options = walk_options(limits);
        let map = repo_map(&root, &root, 3, &options).await.unwrap();
        assert!(map.truncated);
        assert!(map.text.contains("4 entries shown"));
        assert_eq!(map.entries, 4);
    }

    #[tokio::test]
    async fn offset_pages_are_disjoint_and_complete() {
        let h = TempRoot::new("pages").await;
        for name in ["a.txt", "b.txt", "c.txt"] {
            h.put(name, "hit 1\nmiss\nhit 2\n").await;
        }
        let root = h.root();
        let options = walk_options(CodingLimits::default());
        let mut full = input(&root, &root, "hit");
        full.limit = 100;
        let full = search_text(&full, &options).await.unwrap();
        let want: Vec<String> = full.text.split('\n').map(str::to_owned).collect();
        assert_eq!(want.len(), 6);

        let mut got: Vec<String> = Vec::new();
        let mut offset = 0usize;
        for _page in 0..10 {
            let mut page = input(&root, &root, "hit");
            page.limit = 2;
            page.offset = offset;
            let outcome = search_text(&page, &options).await.unwrap();
            let lines: Vec<String> = outcome
                .text
                .split('\n')
                .filter(|line| {
                    matches!(line.chars().next(), Some('a') | Some('b') | Some('c'))
                        && line.get(1..6) == Some(".txt:")
                })
                .map(str::to_owned)
                .collect();
            got.extend(lines.clone());
            let marker = "call again with offset=";
            let Some(at) = outcome.text.find(marker) else {
                break;
            };
            let digits: String = outcome.text[at + marker.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            assert_eq!(lines.len(), 2);
            offset = digits.parse().unwrap();
        }
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn load_ignore_matcher_covers_negation_and_anchoring() {
        let h = TempRoot::new("matcher").await;
        h.put(".gitignore", "*.log\n!keep.log\nsrc/\n/docs/\nsub/a\n")
            .await;
        let root = h.root();
        let matcher = load_ignore_matcher(&root, &root, &CodingLimits::default()).await;
        assert!(matcher.is_ignored("x.log", false));
        assert!(!matcher.is_ignored("keep.log", false));
        assert!(matcher.is_ignored("src", true));
        assert!(!matcher.is_ignored("src", false));
        assert!(matcher.is_ignored("docs", true));
        assert!(!matcher.is_ignored("docs", false));
        // `sub/a` is anchored: it only ignores the root-relative path.
        assert!(matcher.is_ignored("sub/a", false));
        assert!(!matcher.is_ignored("other/sub/a", false));
        assert!(!matcher.is_ignored("plain.txt", false));
    }
}
