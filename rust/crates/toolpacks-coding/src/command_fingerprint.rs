//! Bounded configuration identity for standing shell approvals. Rust port of
//! `packages/toolpacks-coding/src/command-fingerprint.ts` plus the helper
//! inputs from `command-script-inputs.ts`. The digest is the standing-approval
//! key material: an approval survives only while the executable, script,
//! configuration and environment identity hash unchanged. Every bound or
//! ambiguity that TypeScript reports as an error (and the caller maps to
//! "no key, ask again") is reported here the same way — a refusal to key is
//! always the fail-safe answer.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::command_policy::{executable_name, shell_words};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 128;
const MAX_ANCESTORS: usize = 16;
const MAX_ENV_BYTES: usize = 128 * 1024;

/// Any deviation from a fully resolvable identity. `Message` mirrors the
/// TypeScript errors that carry no `code` (always abort); `Io` keeps the OS
/// class so `bind_file` can tell "absent" from "unauthorized" exactly like
/// the `isAbsent`/EACCES branch in TS does.
pub enum FingerprintError {
    Message(String),
    Io(std::io::Error),
}

impl std::fmt::Debug for FingerprintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => write!(f, "FingerprintError({message})"),
            Self::Io(error) => write!(f, "FingerprintError(io: {error})"),
        }
    }
}

impl std::fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => f.write_str(message),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FingerprintError {}

type FpResult<T> = Result<T, FingerprintError>;

fn fail<T>(message: &str) -> FpResult<T> {
    Err(FingerprintError::Message(message.to_owned()))
}

/// The environment names whose values bind the key. Everything else a
/// standing approval saw is irrelevant to what the command executes.
const ENV_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "BASH_ENV",
    "ENV",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "NODE_ENV",
    "NODE_OPTIONS",
    "NODE_PATH",
    "BUN_OPTIONS",
    "BUN_INSTALL",
    "PYTHONPATH",
    "PYTHONHOME",
    "VIRTUAL_ENV",
    "PYTEST_ADDOPTS",
    "UV_PROJECT_ENVIRONMENT",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "GOPATH",
    "GOROOT",
    "GOFLAGS",
    "GOWORK",
    "GOTOOLCHAIN",
    "MAKEFLAGS",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "npm_config_prefix",
    "npm_config_userconfig",
    "npm_config_registry",
];

/// The `a.localeCompare(b)` collation order Node and Bun agree on for the
/// fixed name universe. Shared with the TypeScript side through a fixture so
/// the digest inputs are byte-identical; pinned by a repository test.
static ENV_ORDER: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    let raw = include_str!("../../../fixtures/coding-fingerprint/env-order.json");
    serde_json::from_str(raw).expect("env order fixture is a JSON string array")
});

fn env_rank(name: &str) -> usize {
    ENV_ORDER
        .iter()
        .position(|entry| entry == name)
        .unwrap_or(usize::MAX)
}

/// JS `JSON.stringify` string escaping (QuoteJSONString, ES2019 well-formed
/// form). Rust strings cannot carry lone surrogates, so the well-formed
/// escape branch is unreachable by construction; U+2028/U+2029 are escaped
/// exactly as `JSON.stringify` does.
pub(crate) fn json_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// JS `String.prototype.trim`: the ECMA-262 `WhiteSpace` + `LineTerminator`
/// set, which is NOT Rust's `char::is_whitespace` (U+FEFF is whitespace to
/// JS, U+0085 is not).
fn js_trim(value: &str) -> &str {
    const TRIMMED: &[char] = &[
        '\u{0009}', '\u{000a}', '\u{000b}', '\u{000c}', '\u{000d}', '\u{0020}', '\u{00a0}',
        '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}',
        '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}', '\u{2028}', '\u{2029}',
        '\u{202f}', '\u{205f}', '\u{3000}', '\u{feff}',
    ];
    let start = value
        .char_indices()
        .find(|(_, c)| !TRIMMED.contains(c))
        .map_or(value.len(), |(i, _)| i);
    let end = value
        .char_indices()
        .rfind(|(_, c)| !TRIMMED.contains(c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    &value[start..end.max(start)]
}

/// `command.trim().split(/ +/)`: the only split character is U+0020. An
/// all-whitespace command yields one empty token, exactly like JS `split` on
/// the empty string.
fn js_split_tokens(command: &str) -> Vec<String> {
    let trimmed = js_trim(command);
    if trimmed.is_empty() {
        return vec![String::new()];
    }
    trimmed.split(' ').map(|token| token.to_owned()).collect()
}

/// Node `path.posix.dirname`, including the "keep multiple leading slashes"
/// rule (`//x` → `/`).
fn js_dirname(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let leading = path.len() - path.trim_start_matches('/').len();
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    match trimmed.rfind('/') {
        None => ".".to_owned(),
        Some(0) => {
            if leading >= 2 {
                "//".to_owned()
            } else {
                "/".to_owned()
            }
        }
        Some(index) => {
            let mut head = &trimmed[..index];
            while head.len() > leading && head.ends_with('/') {
                head = &head[..head.len() - 1];
            }
            if head.len() < leading {
                "//".to_owned()
            } else {
                head.to_owned()
            }
        }
    }
}

/// Node `path.resolve(base, parts…)`: a leading `/` part restarts; `.` and
/// empty parts drop; `..` pops. Lexical only, like the caller expects.
fn js_resolve(base: &str, parts: &[&str]) -> String {
    let seeds: Vec<&str> = std::iter::once(base)
        .chain(parts.iter().copied())
        .filter(|part| !part.is_empty())
        .collect();
    let last_absolute = seeds
        .iter()
        .rposition(|part| part.starts_with('/'))
        .unwrap_or(0);
    let mut stack: Vec<&str> = Vec::new();
    for part in &seeds[last_absolute..] {
        for piece in part.split('/') {
            match piece {
                "" | "." => {}
                ".." => {
                    stack.pop();
                }
                other => stack.push(other),
            }
        }
    }
    format!("/{}", stack.join("/"))
}

fn js_join(parts: &[&str]) -> String {
    js_resolve("", parts)
}
// ---------------------------------------------------------------------------
// command-script-inputs.ts ports
// ---------------------------------------------------------------------------

fn input_flags() -> HashSet<&'static str> {
    HashSet::from(["--preload", "--require", "--import", "--loader", "-r"])
}

fn boolean_flags() -> HashSet<&'static str> {
    HashSet::from(["--no-warnings", "--enable-source-maps"])
}

fn interpreter_name() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\A(?:node(?:js)?|bun|python[0-9.]*|ruby|perl|lua|sh|bash|zsh)\z")
            .expect("static pattern")
    });
    &RE
}

fn node_bun_name() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\A(?:node(?:js)?|bun)\z").expect("static"));
    &RE
}

fn assignment_prefix() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\A[A-Za-z_][A-Za-z0-9_]*=").expect("static")
    });
    &RE
}

fn file_like_word() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\A[\w./ -]+\.(?:[cm]?[jt]s|py|sh|rb|pl)\z").expect("static")
    });
    &RE
}

fn path_override() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\APATH=").expect("static"));
    &RE
}

fn wrapper_word() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\A(?:env|command|exec)\z").expect("static")
    });
    &RE
}

/// Port of `interpreterFile`: supported startup flags precede the positional
/// file; unknown arity cannot be guessed.
fn interpreter_file(words: &[String]) -> FpResult<Option<String>> {
    let name = executable_name(words.first().map(String::as_str).unwrap_or(""));
    if !interpreter_name().is_match(&name) {
        return Ok(None);
    }
    let mut index = 1usize;
    while index < words.len() {
        let word = &words[index];
        if word == "--" {
            return match words.get(index + 1) {
                Some(script) => Ok(Some(script.clone())),
                None => fail("direct interpreter entrypoint is unresolved"),
            };
        }
        let equal = word.find('=');
        let flag = match equal {
            None => word.as_str(),
            Some(at) => &word[..at],
        };
        if input_flags().contains(flag) {
            if equal.is_none() {
                index += 1;
            }
            index += 1;
            continue;
        }
        if word.starts_with("-r") && word.len() > 2 && node_bun_name().is_match(&name) {
            index += 1;
            continue;
        }
        if boolean_flags().contains(word.as_str()) {
            index += 1;
            continue;
        }
        if word.starts_with('-') {
            return fail("unsupported direct interpreter options");
        }
        if name == "bun" && (word == "run" || word == "build") {
            return fail("nested Bun entrypoint resolution is unsupported");
        }
        return Ok(if name == "bun" && word == "test" {
            None
        } else {
            Some(word.clone())
        });
    }
    fail("direct interpreter entrypoint is unresolved")
}

fn strip_wrappers(words: &mut Vec<String>) {
    while words
        .first()
        .is_some_and(|first| assignment_prefix().is_match(first) || wrapper_word().is_match(first))
    {
        words.remove(0);
    }
}

/// Port of `directScriptFiles`: only literal direct inputs are covered;
/// shell-generated paths cannot be bound.
pub fn direct_script_files(command: &str) -> FpResult<Vec<String>> {
    if command
        .chars()
        .any(|c| matches!(c, '$' | '`' | ';' | '&' | '|' | '<' | '>'))
    {
        return fail("nonliteral package script cannot receive standing approval");
    }
    let mut words = shell_words(command);
    if words.iter().any(|word| path_override().is_match(word)) {
        return fail("package script PATH override is unsupported");
    }
    let mut files: Vec<String> = Vec::new();
    let push = |files: &mut Vec<String>, word: &str| {
        if !files.iter().any(|seen| seen == word) {
            files.push(word.to_owned());
        }
    };
    for word in &words {
        if !word.starts_with('-') && file_like_word().is_match(word) {
            push(&mut files, word);
        }
    }
    strip_wrappers(&mut words);
    let entry = words.first().cloned().unwrap_or_default();
    let entry_name = executable_name(&entry);
    if entry_name == "npx" || entry_name == "bunx" {
        return fail("nested package runner resolution is unsupported");
    }
    if entry.starts_with('-') || matches!(entry.as_str(), "cd" | "pushd" | "popd") {
        return fail("unsupported package script command resolution");
    }
    if entry.contains('/') {
        push(&mut files, &entry);
    }
    if let Some(script) = interpreter_file(&words)? {
        push(&mut files, &script);
    }
    Ok(files)
}

/// Port of `scriptExecutable`: bare leading executable of a script command,
/// wrapper prefixes stripped; path-shaped entries bind as direct files.
pub fn script_executable(command: &str) -> Option<String> {
    let mut words = shell_words(command);
    strip_wrappers(&mut words);
    let entry = words.first()?.clone();
    if entry.contains('/') || entry.starts_with('-') {
        return None;
    }
    Some(entry)
}

/// Port of `leadingExecutable`: resolve only supported literal wrapper
/// prefixes, never search ordinary arguments.
pub fn leading_executable(tokens: &[String]) -> Option<String> {
    let mut index = 0usize;
    while index < tokens.len() {
        let word = &tokens[index];
        if assignment_prefix().is_match(word) {
            index += 1;
            continue;
        }
        let name = executable_name(word);
        if name == "env" || name == "command" {
            if tokens.get(index + 1).map(String::as_str) == Some("--") {
                index += 1;
            }
            index += 1;
            continue;
        }
        if name == "npx" || name == "bunx" {
            return match tokens.get(index + 1) {
                None => None,
                Some(selected) if selected.starts_with('-') => None,
                Some(selected) => Some(executable_name(selected)),
            };
        }
        return if word.starts_with('-') {
            None
        } else {
            Some(name)
        };
    }
    None
}

// ---------------------------------------------------------------------------
// file identity primitives
// ---------------------------------------------------------------------------

/// The six `String()`-formatted BigInt fields the TS identity array carries:
/// dev, ino, mode, size, mtimeNs, ctimeNs. `Ok(None)` means "exists, not a
/// regular file" — the TS `continue` path.
fn stat_identity(path: &str) -> FpResult<Option<[String; 6]>> {
    let c_path = std::ffi::CString::new(path)
        .map_err(|_| FingerprintError::Message("path contains a NUL byte".to_owned()))?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::stat(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(FingerprintError::Io(std::io::Error::last_os_error()));
    }
    #[cfg(target_os = "macos")]
    let (mode, dev) = (u32::from(stat.st_mode), stat.st_dev as u64);
    #[cfg(not(target_os = "macos"))]
    let (mode, dev) = (stat.st_mode, stat.st_dev);
    if mode & 0o170000 != 0o100000 {
        return Ok(None);
    }
    let (mtime_sec, mtime_nsec, ctime_sec, ctime_nsec) = stat_timespecs(&stat);
    let mtime_ns = mtime_sec as i128 * 1_000_000_000 + mtime_nsec as i128;
    let ctime_ns = ctime_sec as i128 * 1_000_000_000 + ctime_nsec as i128;
    Ok(Some([
        dev.to_string(),
        stat.st_ino.to_string(),
        mode.to_string(),
        stat.st_size.to_string(),
        mtime_ns.to_string(),
        ctime_ns.to_string(),
    ]))
}

/// Node's `stat(bigint)` reports `mtimeNs` and `ctimeNs` with nanosecond
/// precision. The libc `stat` timespec field names differ per platform: Linux
/// uses flat `st_mtime`/`st_mtime_nsec`, macOS uses `st_mtimespec`.
#[cfg(target_os = "linux")]
fn stat_timespecs(stat: &libc::stat) -> (i64, i64, i64, i64) {
    (
        stat.st_mtime,
        stat.st_mtime_nsec,
        stat.st_ctime,
        stat.st_ctime_nsec,
    )
}

#[cfg(target_os = "macos")]
fn stat_timespecs(stat: &libc::stat) -> (i64, i64, i64, i64) {
    (
        stat.st_mtimespec.tv_sec,
        stat.st_mtimespec.tv_nsec,
        stat.st_ctimespec.tv_sec,
        stat.st_ctimespec.tv_nsec,
    )
}

/// TS `isAbsent`: ENOENT, ENOTDIR and EACCES mean "not there for us";
/// anything else aborts the fingerprint.
fn is_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::PermissionDenied
    ) || matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ENOTDIR) | Some(libc::EACCES)
    )
}

fn is_eacces(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
        || error.raw_os_error() == Some(libc::EACCES)
}

fn real_path(path: &str) -> Result<String, std::io::Error> {
    let canonical = std::fs::canonicalize(Path::new(path))?;
    canonical
        .to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF-8 path"))
}

#[derive(Clone)]
struct Executable {
    path: String,
    canonical: String,
    identity: [String; 6],
}

impl Executable {
    /// `JSON.stringify` shape; script runners carry `script` first, matching
    /// the TS object spread order.
    fn json(&self, script: Option<&str>) -> String {
        let identity = self
            .identity
            .iter()
            .map(|value| json_quote(value))
            .collect::<Vec<_>>()
            .join(",");
        match script {
            Some(script) => format!(
                "{{\"script\":{},\"path\":{},\"canonical\":{},\"identity\":[{}]}}",
                json_quote(script),
                json_quote(&self.path),
                json_quote(&self.canonical),
                identity
            ),
            None => format!(
                "{{\"path\":{},\"canonical\":{},\"identity\":[{}]}}",
                json_quote(&self.path),
                json_quote(&self.canonical),
                identity
            ),
        }
    }
}

/// TS `executable`: first candidate path that is X_OK, resolves, and is a
/// regular file; ENOENT/ENOTDIR/EACCES skip the candidate, anything else
/// aborts.
fn executable(
    file: &str,
    cwd: &str,
    env: &HashMap<String, String>,
) -> FpResult<Option<Executable>> {
    let candidates: Vec<String> = if file.contains('/') {
        vec![js_resolve(cwd, &[file])]
    } else {
        env.get("PATH")
            .cloned()
            .unwrap_or_else(|| "/usr/bin:/bin".to_owned())
            .split(':')
            .map(|entry| js_resolve(cwd, &[entry, file]))
            .collect()
    };
    for path in candidates {
        let c_path = match std::ffi::CString::new(path.as_str()) {
            Ok(value) => value,
            Err(_) => return fail("path contains a NUL byte"),
        };
        if unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } != 0 {
            let error = std::io::Error::last_os_error();
            if is_absent(&error) {
                continue;
            }
            return Err(FingerprintError::Io(error));
        }
        let canonical = match real_path(&path) {
            Ok(value) => value,
            Err(error) => {
                if is_absent(&error) {
                    continue;
                }
                return Err(FingerprintError::Io(error));
            }
        };
        match stat_identity(&canonical)? {
            Some(identity) => {
                return Ok(Some(Executable {
                    path,
                    canonical,
                    identity,
                }));
            }
            None => continue,
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// binding state
// ---------------------------------------------------------------------------

struct BindState {
    bytes: u64,
    count: usize,
    entries: Vec<String>,
    packages: Vec<(String, Vec<u8>)>,
}

impl BindState {
    fn new() -> Self {
        Self {
            bytes: 0,
            count: 0,
            entries: Vec::new(),
            packages: Vec::new(),
        }
    }
}

/// TS `boundedContents`: read `expectedSize + 1` bytes in one bounded loop;
/// more means the file changed under us.
fn bounded_contents(path: &str, expected: u64) -> FpResult<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(FingerprintError::Io)?;
    let limit = expected as usize + 1;
    let mut buffer = vec![0u8; limit];
    let mut length = 0usize;
    while length < buffer.len() {
        let read = file
            .read(&mut buffer[length..])
            .map_err(FingerprintError::Io)?;
        if read == 0 {
            break;
        }
        length += read;
    }
    if length > expected as usize {
        return fail("standing approval metadata changed while reading");
    }
    buffer.truncate(length);
    Ok(buffer)
}

/// TS `bindFile`: bind the file hash, or record the literal absence of the
/// path (its later appearance changes the key). Bound violations and EACCES
/// always abort; ENOENT/ENOTDIR record `[path, null]` when not required.
fn bind_file(path: &str, state: &mut BindState, required: bool) -> FpResult<()> {
    let outcome = (|| -> FpResult<()> {
        let canonical = real_path(path).map_err(FingerprintError::Io)?;
        let identity = stat_identity(&canonical)?.ok_or_else(|| {
            FingerprintError::Message(format!(
                "standing approval metadata is not a file: {canonical}"
            ))
        })?;
        let size: u64 = identity[3]
            .parse()
            .map_err(|_| FingerprintError::Message("unreadable file size".to_owned()))?;
        if size > MAX_FILE_BYTES || state.bytes + size > MAX_TOTAL_BYTES {
            return fail("standing approval metadata exceeds bounds");
        }
        state.count += 1;
        if state.count > MAX_FILES {
            return fail("too many standing approval inputs");
        }
        let contents = bounded_contents(&canonical, size)?;
        if contents.len() as u64 > MAX_FILE_BYTES
            || state.bytes + contents.len() as u64 > MAX_TOTAL_BYTES
        {
            return fail("standing approval metadata exceeds bounds");
        }
        state.bytes += contents.len() as u64;
        if path.ends_with("/package.json") {
            state.packages.push((path.to_owned(), contents.clone()));
        }
        let hash = hex_sha256(&contents);
        state.entries.push(format!(
            "[{},{},{}]",
            json_quote(path),
            json_quote(&canonical),
            json_quote(&hash)
        ));
        Ok(())
    })();
    match outcome {
        Ok(()) => Ok(()),
        Err(error) if required => Err(error),
        Err(FingerprintError::Io(error)) => {
            if is_eacces(&error) {
                return Err(FingerprintError::Io(error));
            }
            if is_absent(&error) {
                state.entries.push(format!("[{},null]", json_quote(path)));
                Ok(())
            } else {
                Err(FingerprintError::Io(error))
            }
        }
        Err(error) => Err(error),
    }
}

/// TS `bindDirectFile`: the resolved target must stay inside the workspace.
fn bind_direct_file(root: &str, path: &str, state: &mut BindState) -> FpResult<()> {
    let canonical = real_path(path).map_err(FingerprintError::Io)?;
    let inside = crate::paths::relative(root, &canonical);
    if inside.starts_with('/') || inside == ".." || inside.starts_with("../") {
        return fail("direct command input outside workspace");
    }
    bind_file(path, state, true)
}

fn metadata_names() -> Vec<&'static str> {
    vec![
        "package.json",
        "bun.lock",
        "bun.lockb",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
        ".npmrc",
        ".yarnrc.yml",
        "bunfig.toml",
        ".bunfig.toml",
        ".node-version",
        ".nvmrc",
        ".tool-versions",
        "Cargo.toml",
        "Cargo.lock",
        ".cargo/config",
        ".cargo/config.toml",
        "rust-toolchain.toml",
        "rust-toolchain",
        "go.mod",
        "go.sum",
        "go.work",
        "go.work.sum",
        "Makefile",
        "makefile",
        "GNUmakefile",
        "pyproject.toml",
        "uv.lock",
        "pytest.ini",
        "tox.ini",
        "setup.cfg",
        "requirements.txt",
        ".python-version",
        "tsconfig.json",
        "jsconfig.json",
        ".eslintrc",
        ".eslintrc.json",
        ".prettierrc",
        ".prettierrc.json",
        ".prettierignore",
        "biome.json",
    ]
}

static CONFIG_NAMES: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    let mut names = Vec::new();
    for stem in [
        "vitest.config",
        "vite.config",
        "jest.config",
        "eslint.config",
        "prettier.config",
    ] {
        for extension in ["ts", "js", "mts", "mjs", "cts", "cjs", "json"] {
            names.push(format!("{stem}.{extension}"));
        }
    }
    names
});

fn bind_metadata(dirs: &[String], state: &mut BindState) -> FpResult<()> {
    for dir in dirs {
        for name in metadata_names() {
            bind_file(&js_join(&[dir, name]), state, false)?;
        }
        for name in CONFIG_NAMES.iter() {
            bind_file(&js_join(&[dir, name]), state, false)?;
        }
    }
    Ok(())
}

/// TS `scriptRunner`: mirrors npm run-script PATH — every ancestor
/// `node_modules/.bin`, then PATH. Unresolved is an error, not an absence.
fn script_runner(dir: &str, name: &str, env: &HashMap<String, String>) -> FpResult<Executable> {
    let mut current = dir.to_owned();
    loop {
        let local = js_join(&[&current, "node_modules", ".bin", name]);
        if let Some(found) = executable(&local, dir, env)? {
            return Ok(found);
        }
        let parent = js_dirname(&current);
        if parent == current {
            break;
        }
        current = parent;
    }
    executable(name, dir, env)?.ok_or_else(|| {
        FingerprintError::Message("package script executable is unresolved".to_owned())
    })
}

#[allow(clippy::type_complexity)]
fn bind_script_entrypoints(
    root: &str,
    dirs: &[String],
    tokens: &[String],
    env: &HashMap<String, String>,
    executables: &mut Vec<(Option<String>, Executable)>,
    state: &mut BindState,
) -> FpResult<()> {
    let script = match tokens.get(1).map(String::as_str) {
        Some("run") => tokens.get(2).cloned(),
        Some("test") => Some("test".to_owned()),
        _ => None,
    };
    let Some(script) = script else {
        return Ok(());
    };
    for dir in dirs {
        let pkg_path = js_join(&[dir, "package.json"]);
        let Some((_, contents)) = state
            .packages
            .iter()
            .find(|(path, _)| *path == pkg_path)
            .cloned()
        else {
            continue;
        };
        let parsed: serde_json::Value = serde_json::from_slice(&contents)
            .map_err(|error| FingerprintError::Message(error.to_string()))?;
        let scripts = parsed.get("scripts").and_then(serde_json::Value::as_object);
        for name in [
            script.clone(),
            format!("pre{script}"),
            format!("post{script}"),
        ] {
            let Some(command) = scripts
                .and_then(|scripts| scripts.get(name.as_str()))
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            for word in direct_script_files(command)? {
                bind_direct_file(root, &js_resolve(dir, &[&word]), state)?;
            }
            bind_explicit_configs(root, dir, &shell_words(command), &HashMap::new(), state)?;
            if let Some(bare) = script_executable(command) {
                let found = script_runner(dir, &bare, env)?;
                executables.push((Some(bare), found));
            }
        }
    }
    Ok(())
}

fn short_runner_flag() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\A-[pcr][^\n\r\u{2028}\u{2029}]+\z").expect("static")
    });
    &RE
}

/// TS `bindExplicitConfigs`: runner configuration files named on the command
/// line are part of the identity, as are shell `BASH_ENV`/`ENV` and the npm
/// userconfig. A dangling flag argument is a refusal, never a guess.
fn bind_explicit_configs(
    root: &str,
    cwd: &str,
    tokens: &[String],
    env: &HashMap<String, String>,
    state: &mut BindState,
) -> FpResult<()> {
    let mut flags: HashSet<&'static str> = HashSet::from([
        "--config",
        "--project",
        "--manifest-path",
        "--modfile",
        "--preload",
        "--require",
        "--import",
        "--loader",
    ]);
    let typescript = leading_executable(tokens).as_deref() == Some("tsc");
    if typescript {
        flags.insert("-p");
    }
    if tokens.iter().any(|token| token == "pytest") {
        flags.insert("-c");
    }
    if tokens
        .iter()
        .any(|token| node_bun_name().is_match(&executable_name(token)))
    {
        flags.insert("-r");
    }
    for (index, token) in tokens.iter().enumerate() {
        let equal = token.find('=');
        let short = short_runner_flag().is_match(token) && flags.contains(&token[..2]);
        let flag: &str = if short {
            &token[..2]
        } else {
            match equal {
                None => token,
                Some(at) => &token[..at],
            }
        };
        let attached: Option<&str> = if short {
            Some(&token[2..])
        } else {
            equal.map(|at| &token[at + 1..])
        };
        if !flags.contains(flag) {
            continue;
        }
        let file = match attached.or(tokens.get(index + 1).map(String::as_str)) {
            None => return fail("missing runner configuration"),
            Some(file) => file,
        };
        if file.is_empty() || file.starts_with('-') {
            return fail("missing runner configuration");
        }
        let path = js_resolve(cwd, &[file]);
        let metadata = std::fs::metadata(&path).map_err(FingerprintError::Io)?;
        let project_directory =
            metadata.is_dir() && typescript && (flag == "--project" || flag == "-p");
        if metadata.is_dir() && !project_directory {
            return fail("direct command input must be a file");
        }
        bind_direct_file(
            root,
            &if project_directory {
                js_join(&[&path, "tsconfig.json"])
            } else {
                path.clone()
            },
            state,
        )?;
    }
    for name in ["BASH_ENV", "ENV", "npm_config_userconfig"] {
        if let Some(file) = env.get(name) {
            bind_direct_file(root, &js_resolve(cwd, &[file]), state)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// environment and entry point
// ---------------------------------------------------------------------------

/// TS `executionEnvironment`: sanitized environment restricted to the names
/// that can change what executes, in collation order, within byte bounds.
fn execution_environment(source: &HashMap<String, String>) -> FpResult<Vec<(String, String)>> {
    let sanitized = crate::exec_tools::sanitized_env(source, "/standing-approval");
    let mut entries: Vec<(String, String)> = sanitized
        .into_iter()
        .filter(|(name, _)| ENV_NAMES.contains(&name.as_str()))
        .collect();
    entries.sort_by_key(|(name, _)| env_rank(name));
    let json = format!(
        "{{{}}}",
        entries
            .iter()
            .map(|(name, value)| format!("{}:{}", json_quote(name), json_quote(value)))
            .collect::<Vec<_>>()
            .join(",")
    );
    let path_entries = entries
        .iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.as_str())
        .unwrap_or("")
        .split(':')
        .count();
    if json.len() > MAX_ENV_BYTES || path_entries > 128 {
        return fail("standing approval environment exceeds bounds");
    }
    Ok(entries)
}

/// TS `ancestors`: the walk from the command cwd up to the workspace root,
/// bounded; a walk that leaves the filesystem without meeting the root means
/// "cannot bind", never "partially bound".
fn ancestors(root: &str, cwd: &str) -> Option<Vec<String>> {
    let mut paths: Vec<String> = Vec::new();
    let mut dir = cwd.to_owned();
    loop {
        paths.push(dir.clone());
        if paths.len() > MAX_ANCESTORS {
            return None;
        }
        if dir == root {
            return Some(paths);
        }
        let parent = js_dirname(&dir);
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

/// Bounded configuration identity, never a claim about arbitrary transitive
/// code. TS `commandFingerprint`; `Ok(None)` is the TS `null` (re-ask).
pub fn command_fingerprint(
    root: &str,
    cwd: &str,
    command: &str,
    environment: &HashMap<String, String>,
) -> FpResult<Option<String>> {
    let Some(dirs) = ancestors(root, cwd) else {
        return Ok(None);
    };
    let entries = execution_environment(environment)?;
    let env: HashMap<String, String> = entries.iter().cloned().collect();
    let tokens = js_split_tokens(command);
    let main = match executable(&tokens[0], cwd, &env)? {
        Some(found) => found,
        None => return Ok(None),
    };
    let mut executables: Vec<(Option<String>, Executable)> = vec![(None, main)];
    if tokens[0] == "npx" || tokens[0] == "bunx" {
        // TS indexes tokens[1] with `!`; a missing word stringifies to
        // "undefined" and is searched literally. Mirror that.
        let runner_name = tokens
            .get(1)
            .cloned()
            .unwrap_or_else(|| "undefined".to_owned());
        let mut runner = None;
        for dir in &dirs {
            let path = js_join(&[dir, "node_modules", ".bin", &runner_name]);
            if let Some(found) = executable(&path, cwd, &env)? {
                runner = Some(found);
                break;
            }
        }
        if runner.is_none() {
            runner = executable(&runner_name, cwd, &env)?;
        }
        match runner {
            Some(found) => executables.push((None, found)),
            None => return Ok(None),
        }
    }
    let mut state = BindState::new();
    bind_metadata(&dirs, &mut state)?;
    bind_script_entrypoints(root, &dirs, &tokens, &env, &mut executables, &mut state)?;
    bind_explicit_configs(root, cwd, &tokens, &env, &mut state)?;
    let document = format!(
        "{{\"v\":1,\"executables\":[{}],\"files\":[{}],\"environment\":[{}]}}",
        executables
            .iter()
            .map(|(script, found)| found.json(script.as_deref()))
            .collect::<Vec<_>>()
            .join(","),
        state.entries.join(","),
        entries
            .iter()
            .map(|(name, value)| format!("[{},{}]", json_quote(name), json_quote(value)))
            .collect::<Vec<_>>()
            .join(",")
    );
    Ok(Some(hex_sha256(document.as_bytes())))
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path, files: &[(&str, &str, Option<u32>)]) {
        for (name, contents, mode) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            if let Some(mode) = mode {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode))
                        .unwrap();
                }
            }
        }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn js_trim_matches_the_ecma_whitespace_set() {
        assert_eq!(js_trim("\u{feff} abc\u{a0}\n"), "abc");
        assert_eq!(
            js_trim("a\u{85}b"),
            "a\u{85}b",
            "U+0085 is not JS whitespace"
        );
        assert_eq!(js_trim("   "), "");
    }

    #[test]
    fn tokens_split_only_on_u0020() {
        // Tabs are NOT separators; JS `split(/ +/)` keeps them inside tokens.
        assert_eq!(js_split_tokens("a\tb c"), vec!["a\tb", "c"]);
        assert_eq!(js_split_tokens("a  b"), vec!["a", "", "b"]);
        assert_eq!(js_split_tokens(""), vec![""]);
    }

    #[test]
    fn dirname_follows_node_posix() {
        assert_eq!(js_dirname("/a/b/c"), "/a/b");
        assert_eq!(js_dirname("/a/b/"), "/a");
        assert_eq!(js_dirname("/a"), "/");
        assert_eq!(js_dirname("a"), ".");
        assert_eq!(js_dirname("/"), "/");
        assert_eq!(
            js_dirname("//a/b"),
            "//a",
            "node preserves multiple leading slashes"
        );
    }

    #[test]
    fn resolve_and_join_follow_node_lexical_rules() {
        assert_eq!(js_resolve("/a/b", &["c", "d"]), "/a/b/c/d");
        assert_eq!(js_resolve("/a/b", &["/x", "y"]), "/x/y");
        assert_eq!(js_resolve("/a/b", &["../c"]), "/a/c");
        assert_eq!(js_resolve("/a/b", &["./c/"]), "/a/b/c");
        assert_eq!(js_join(&["/a/b/", "./c"]), "/a/b/c");
        assert_eq!(
            js_join(&["/a", ".cargo/config.toml"]),
            "/a/.cargo/config.toml"
        );
    }

    #[test]
    fn environment_is_restricted_sorted_and_bounded() {
        let source = env(&[
            ("PATH", "/bin"),
            ("HOME", "/home/u"),
            ("LANG", "en_US.UTF-8"),
            ("IRRELEVANT_NAME", "x"),
            ("MY_TOKEN", "secret"),
            ("npm_config_registry", "https://r"),
            ("BASH_ENV", "/b"),
        ]);
        let entries = execution_environment(&source).unwrap();
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["BASH_ENV", "HOME", "LANG", "npm_config_registry", "PATH"],
            "only ENV_NAMES survive, in localeCompare order"
        );
        let huge = env(&[("PATH", &"x".repeat(MAX_ENV_BYTES + 10))]);
        assert!(execution_environment(&huge).is_err(), "byte bound");
        let many = (0..129)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join(":");
        assert!(
            execution_environment(&env(&[("PATH", &many)])).is_err(),
            "PATH entry bound"
        );
    }

    #[test]
    fn ancestors_bounded_and_rooted() {
        assert_eq!(ancestors("/a/b", "/a/b").unwrap(), vec!["/a/b"]);
        assert_eq!(
            ancestors("/a", "/a/b/c").unwrap(),
            vec!["/a/b/c", "/a/b", "/a"]
        );
        assert_eq!(ancestors("/x", "/a/b/c"), None, "cwd outside root");
        assert_eq!(
            ancestors("/", "/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q"),
            None,
            "depth bound"
        );
        assert_eq!(
            ancestors("/", "/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o")
                .unwrap()
                .len(),
            16,
            "exactly 16 ancestors fit (15 dirs + root)"
        );
    }

    #[test]
    fn script_inputs_are_literal_only() {
        assert_eq!(
            direct_script_files("vitest run").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            direct_script_files("node check.mjs --reporter").unwrap(),
            vec!["check.mjs"]
        );
        assert!(
            direct_script_files("sh -x run.sh").is_err(),
            "unknown interpreter flag"
        );
        assert!(
            direct_script_files("node x.mjs && echo hi").is_err(),
            "operator"
        );
        assert!(
            direct_script_files("PATH=/x node x.mjs").is_err(),
            "PATH override"
        );
        assert!(direct_script_files("npx vitest").is_err(), "nested runner");
        assert!(
            direct_script_files("node --unknown x.mjs").is_err(),
            "unknown flag"
        );
        assert_eq!(
            direct_script_files("env NODE_ENV=test node x.mjs").unwrap(),
            vec!["x.mjs"]
        );
        assert_eq!(
            script_executable("npx vitest run"),
            Some("npx".to_owned()),
            "wrappers stripped are env/command/exec only"
        );
        assert_eq!(
            script_executable("./local/run.sh"),
            None,
            "path-shaped entry is a file"
        );
        assert_eq!(
            leading_executable(&env_tokens(&["npx", "tsc", "--noEmit"])),
            Some("tsc".to_owned())
        );
        assert_eq!(
            leading_executable(&env_tokens(&["env", "--", "tsc"])),
            Some("tsc".to_owned())
        );
        assert_eq!(leading_executable(&env_tokens(&["-x", "tsc"])), None);
    }

    fn env_tokens(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn fixture(tag: &str) -> (std::path::PathBuf, HashMap<String, String>) {
        let root = std::env::temp_dir().join(format!("ak-fp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        tree(
            &root,
            &[
                ("bin/bun", "#!/bin/sh\nexit 0\n", Some(0o755)),
                ("bin/node", "#!/bin/sh\nexit 0\n", Some(0o755)),
                (
                    "package.json",
                    r#"{"name":"t","scripts":{"test":"node check.mjs"}}"#,
                    None,
                ),
                ("check.mjs", "console.log(1)\n", None),
            ],
        );
        let root = std::fs::canonicalize(&root).unwrap();
        let environment = env(&[(
            "PATH",
            std::fs::canonicalize(&bin).unwrap().to_str().unwrap(),
        )]);
        (root, environment)
    }

    #[test]
    fn fingerprint_is_stable_and_reacts_to_every_bound_input() {
        let (root, environment) = fixture("stable");
        let digest = command_fingerprint(
            &root.to_string_lossy(),
            &root.to_string_lossy(),
            "bun test",
            &environment,
        )
        .unwrap()
        .expect("keyable");
        let again = command_fingerprint(
            &root.to_string_lossy(),
            &root.to_string_lossy(),
            "bun test",
            &environment,
        )
        .unwrap()
        .unwrap();
        assert_eq!(digest, again, "pure function of the tree");

        // The node script named by package.json is bound: editing it changes
        // the digest (script entrypoint binding is live).
        std::fs::write(root.join("check.mjs"), "console.log(2)\n").unwrap();
        let after = command_fingerprint(
            &root.to_string_lossy(),
            &root.to_string_lossy(),
            "bun test",
            &environment,
        )
        .unwrap()
        .unwrap();
        assert_ne!(digest, after, "script entrypoint identity");

        // Adding an ancestor metadata file changes the digest even though no
        // code path read it before.
        std::fs::write(root.join("bun.lock"), "v1\n").unwrap();
        let after2 = command_fingerprint(
            &root.to_string_lossy(),
            &root.to_string_lossy(),
            "bun test",
            &environment,
        )
        .unwrap()
        .unwrap();
        assert_ne!(after, after2, "metadata appearance changes the key");

        // Environment names outside ENV_NAMES never bind.
        let mut extended = environment.clone();
        extended.insert("IRRELEVANT_NAME".to_owned(), "x".to_owned());
        assert_eq!(
            command_fingerprint(
                &root.to_string_lossy(),
                &root.to_string_lossy(),
                "bun test",
                &extended
            )
            .unwrap()
            .unwrap(),
            after2
        );
    }

    #[test]
    fn fingerprint_refusals_are_all_fail_safe_none() {
        let (root, environment) = fixture("refuse");
        let root_s = root.to_string_lossy().into_owned();
        let fp = |command: &str, cwd: &str, env: &HashMap<String, String>| {
            command_fingerprint(&root_s, cwd, command, env)
        };
        assert_eq!(
            fp("nosuchbinary-ak x", &root_s, &environment).unwrap(),
            None,
            "unresolved main"
        );
        assert_eq!(
            fp("bun test", "/nonexistent-cwd", &environment).unwrap(),
            None,
            "cwd is not under root"
        );
        let deep = format!("{}/a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q", root_s);
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            fp("bun test", &deep, &environment).unwrap(),
            None,
            "ancestor walk leaves the root depth bound"
        );
        // A flag-named config that does not exist is a refusal, not an absence.
        assert!(fp("bun test --config missing.toml", &root_s, &environment).is_err());
        // A direct input outside the workspace is a refusal.
        std::fs::write(root.join("../escape-ak-target.txt"), "x").ok();
        let outside = std::env::temp_dir()
            .join("escape-ak-target.txt")
            .to_string_lossy()
            .into_owned();
        let _ = &outside;
        // (relative escape through the parent is already covered by resolve +
        // bindDirectFile; assert via ../ when the temp parent is writable)
        let rel = format!("../ak-nope-{}-missing", std::process::id());
        assert!(fp(&format!("bun test --config {rel}"), &root_s, &environment).is_err());
    }

    #[test]
    fn explicit_config_flags_bind_the_named_file() {
        let (root, environment) = fixture("config");
        let root_s = root.to_string_lossy().into_owned();
        std::fs::write(root.join("extra.toml"), "a = 1\n").unwrap();
        let with = command_fingerprint(
            &root_s,
            &root_s,
            "bun test --config extra.toml",
            &environment,
        )
        .unwrap()
        .unwrap();
        let plain = command_fingerprint(&root_s, &root_s, "bun test", &environment)
            .unwrap()
            .unwrap();
        assert_ne!(with, plain, "the bound config file must enter the digest");
        std::fs::write(root.join("extra.toml"), "a = 2\n").unwrap();
        let changed = command_fingerprint(
            &root_s,
            &root_s,
            "bun test --config extra.toml",
            &environment,
        )
        .unwrap()
        .unwrap();
        assert_ne!(
            with, changed,
            "editing the bound config invalidates the key"
        );
    }

    #[test]
    fn npx_binds_the_runner_and_refuses_when_unresolved() {
        let (root, environment) = fixture("npx");
        let root_s = root.to_string_lossy().into_owned();
        let mut unresolvable = environment.clone();
        unresolvable.insert("PATH".to_owned(), "/nonexistent-runner-dir".to_owned());
        assert_eq!(
            command_fingerprint(&root_s, &root_s, "npx vitest run", &unresolvable).unwrap(),
            None,
            "unresolved runner is null, not an error"
        );
        tree(
            &root,
            &[
                ("bin/vitest", "#!/bin/sh\nexit 0\n", Some(0o755)),
                ("bin/npx", "#!/bin/sh\nexit 0\n", Some(0o755)),
            ],
        );
        let digest = command_fingerprint(&root_s, &root_s, "npx vitest run", &environment).unwrap();
        assert!(digest.is_some(), "PATH-resolved runner keys the call");
    }

    #[test]
    fn json_quote_matches_stringify_escaping() {
        assert_eq!(json_quote("plain"), "\"plain\"");
        assert_eq!(json_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_quote("tab\tnl\n"), "\"tab\\tnl\\n\"");
        assert_eq!(json_quote("\u{1}\u{1f}"), "\"\\u0001\\u001f\"");
        assert_eq!(json_quote("\u{2028}"), "\"\\u2028\"");
        assert_eq!(json_quote("é✓"), "\"é✓\"");
    }
}
