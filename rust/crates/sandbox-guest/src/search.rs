//! Guest-owned search engine (§8.2), walked exclusively through verified
//! dirfds. Budgets: entry count, 1 MiB/file, 128 MiB total, wall-clock
//! timeout, match cap, 1024-char line cut; `.gitignore` honored in-root only;
//! binary sniffing skips non-text files.

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::Instant;

use agentkit_sandbox_contracts::api::{SearchMatch, SearchQuery, SearchReply};
use agentkit_sandbox_contracts::limits::{MAX_SEARCH_ENTRIES, MAX_SEARCH_MATCHES};
use regex::bytes::{Regex, RegexBuilder};

use crate::error::{GuestFail, code};
use crate::fsops::{GuestFs, cstr, openat2};
use crate::glob::glob_match_path;

const PER_FILE_BYTES: u64 = 1024 * 1024;
const TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const LINE_CHAR_CUT: usize = 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Compiled matcher for the query pattern (literal substring or regex).
enum Matcher {
    Literal(Vec<u8>),
    LiteralCi(Vec<u8>),
    Regex(Regex),
}

impl Matcher {
    fn find_in(&self, haystack: &[u8]) -> bool {
        match self {
            Self::Literal(p) => contains(haystack, p),
            Self::LiteralCi(p) => {
                let lower = haystack.to_ascii_lowercase();
                contains(&lower, p)
            }
            Self::Regex(r) => r.is_match(haystack),
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

struct IgnoreRule {
    base: String,
    pattern: String,
    negate: bool,
    directory_only: bool,
}

/// Entry point for `GuestFsOp::Search`.
pub fn search(fs: &GuestFs, query: &SearchQuery) -> Result<SearchReply, GuestFail> {
    let root = query.root.root();
    let comps: Vec<String> = query.root.components().map(str::to_string).collect();
    let root_fd = fs.open_dir(root, &comps)?;

    let matcher = build_matcher(query)?;
    let glob_re = query.glob.as_deref().map(|g| g.to_string());

    let max_entries = usize::try_from(query.max_entries)
        .unwrap_or(MAX_SEARCH_ENTRIES)
        .min(MAX_SEARCH_ENTRIES);
    let max_matches = usize::try_from(query.max_matches)
        .unwrap_or(MAX_SEARCH_MATCHES)
        .min(MAX_SEARCH_MATCHES);
    let timeout = std::time::Duration::from_millis(query.timeout_ms.min(120_000));
    let deadline = Instant::now() + timeout;

    let mut st = SearchState {
        files_scanned: 0,
        entries_seen: 0,
        total_bytes: 0,
        matches: Vec::new(),
        truncated: false,
        timed_out: false,
        ignores: Vec::new(),
    };
    walk(
        fs,
        root_fd.as_raw_fd(),
        "",
        &matcher,
        glob_re.as_deref(),
        query.context as usize,
        max_entries,
        max_matches,
        deadline,
        &mut st,
    )?;
    if st.entries_seen >= max_entries && !st.timed_out {
        st.truncated = true;
    }
    Ok(SearchReply {
        matches: st.matches,
        files_scanned: st.files_scanned,
        truncated: st.truncated,
        timed_out: st.timed_out,
    })
}

struct SearchState {
    files_scanned: u32,
    entries_seen: usize,
    total_bytes: u64,
    matches: Vec<SearchMatch>,
    truncated: bool,
    timed_out: bool,
    ignores: Vec<IgnoreRule>,
}

fn build_matcher(query: &SearchQuery) -> Result<Matcher, GuestFail> {
    if query.literal {
        return Ok(if query.ignore_case {
            Matcher::LiteralCi(query.pattern.as_bytes().to_ascii_lowercase())
        } else {
            Matcher::Literal(query.pattern.as_bytes().to_vec())
        });
    }
    // Untrusted user regex: bounded compile, no unicode panics (bytes mode).
    let re = RegexBuilder::new(&query.pattern)
        .case_insensitive(query.ignore_case)
        .unicode(false)
        .build()
        .map_err(|_| GuestFail::new(code::BAD_INPUT, "invalid regex pattern"))?;
    Ok(Matcher::Regex(re))
}

#[allow(clippy::too_many_arguments)]
fn walk(
    _fs: &GuestFs,
    dir_fd: RawFd,
    rel_prefix: &str,
    matcher: &Matcher,
    glob: Option<&str>,
    context: usize,
    max_entries: usize,
    max_matches: usize,
    deadline: Instant,
    st: &mut SearchState,
) -> Result<(), GuestFail> {
    // Load this directory's .gitignore (in-root only).
    let dir_ignores = load_gitignore(dir_fd, rel_prefix, st);

    let mut dirs: Vec<String> = Vec::new();
    for name in readdir_names(dir_fd)? {
        if st.timed_out {
            return Ok(());
        }
        if Instant::now() >= deadline {
            st.timed_out = true;
            return Ok(());
        }
        st.entries_seen += 1;
        if st.entries_seen > max_entries {
            st.truncated = true;
            return Ok(());
        }
        if name == "." || name == ".." {
            continue;
        }
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}/{name}")
        };
        if is_ignored(&st.ignores, &dir_ignores, &rel, false) {
            continue;
        }
        let Ok(name_c) = cstr(&name) else { continue };
        let mut sb = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: valid dirfd + NUL name.
        let rc = unsafe {
            libc::fstatat(
                dir_fd,
                name_c.as_ptr(),
                sb.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            continue;
        }
        // SAFETY: fstatat succeeded.
        let sb = unsafe { sb.assume_init() };
        let mode = sb.st_mode & libc::S_IFMT;
        if mode == libc::S_IFDIR {
            if is_ignored(&st.ignores, &dir_ignores, &rel, true) {
                continue;
            }
            dirs.push(name);
            continue;
        }
        if mode != libc::S_IFREG {
            continue; // symlinks/devices/fifos never searched
        }
        if let Some(g) = glob
            && !glob_match_path(g, &rel)
        {
            continue;
        }
        if sb.st_size.max(0) as u64 > PER_FILE_BYTES {
            continue; // over per-file budget
        }
        if st.total_bytes >= TOTAL_BYTES {
            st.truncated = true;
            return Ok(());
        }
        let fd = match openat2(
            dir_fd,
            &name_c,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0,
        ) {
            Ok(fd) => fd,
            Err(_) => continue,
        };
        let data = read_capped(fd, PER_FILE_BYTES, &mut st.total_bytes)?;
        st.files_scanned = st.files_scanned.saturating_add(1);
        if is_binary(&data) {
            continue;
        }
        scan_bytes(&data, matcher, &rel, context, max_matches, st);
        if st.matches.len() >= max_matches {
            st.truncated = true;
            return Ok(());
        }
    }
    for name in dirs {
        if st.timed_out || st.matches.len() >= max_matches {
            return Ok(());
        }
        let Ok(name_c) = cstr(&name) else { continue };
        let next = match openat2(
            dir_fd,
            &name_c,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(fd) => fd,
            Err(_) => continue,
        };
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}/{name}")
        };
        walk(
            _fs,
            next.as_raw_fd(),
            &rel,
            matcher,
            glob,
            context,
            max_entries,
            max_matches,
            deadline,
            st,
        )?;
    }
    Ok(())
}

fn readdir_names(dir_fd: RawFd) -> Result<Vec<String>, GuestFail> {
    // SAFETY: dup + fdopendir; the duped fd is owned by the DIR on success.
    let dupfd = unsafe { libc::dup(dir_fd) };
    if dupfd < 0 {
        return Err(GuestFail::io("dup", &std::io::Error::last_os_error()));
    }
    let dp = unsafe { libc::fdopendir(dupfd) };
    if dp.is_null() {
        // SAFETY: fdopendir failed; close the dup we still own.
        unsafe { libc::close(dupfd) };
        return Err(GuestFail::io("fdopendir", &std::io::Error::last_os_error()));
    }
    let mut out = Vec::new();
    loop {
        // SAFETY: valid DIR.
        let ent = unsafe { libc::readdir(dp) };
        if ent.is_null() {
            break;
        }
        // SAFETY: dirent valid until next readdir.
        let d = unsafe { &*ent };
        // SAFETY: NUL-terminated d_name.
        let name = unsafe { std::ffi::CStr::from_ptr(d.d_name.as_ptr()) };
        if let Ok(s) = name.to_str()
            && s != "."
            && s != ".."
        {
            out.push(s.to_string());
        }
    }
    // SAFETY: closedir.
    unsafe { libc::closedir(dp) };
    out.sort();
    Ok(out)
}

fn read_capped(fd: OwnedFd, cap: u64, total: &mut u64) -> Result<Vec<u8>, GuestFail> {
    let mut out: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut off: i64 = 0;
    loop {
        // SAFETY: pread into initialized buffer.
        let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), off) };
        if n <= 0 {
            break;
        }
        let n = usize::try_from(n).unwrap_or(0);
        off += n as i64;
        out.extend_from_slice(&buf[..n]);
        *total += n as u64;
        if out.len() as u64 >= cap {
            break;
        }
    }
    Ok(out)
}

fn is_binary(data: &[u8]) -> bool {
    let head = &data[..data.len().min(BINARY_SNIFF_BYTES)];
    head.contains(&0) || std::str::from_utf8(data).is_err()
}

fn scan_bytes(
    data: &[u8],
    matcher: &Matcher,
    rel: &str,
    context: usize,
    max_matches: usize,
    st: &mut SearchState,
) {
    let lines: Vec<&[u8]> = data.split(|b| *b == b'\n').collect();
    for (idx, line) in lines.iter().enumerate() {
        if !matcher.find_in(line) {
            continue;
        }
        let before: Vec<String> = lines
            .idx_sub(idx.saturating_sub(context), idx)
            .iter()
            .map(|l| cut_line(l))
            .collect();
        let after_start = idx + 1;
        let after: Vec<String> = if after_start < lines.len() {
            lines
                .idx_sub(after_start, (after_start + context).min(lines.len()))
                .iter()
                .map(|l| cut_line(l))
                .collect()
        } else {
            Vec::new()
        };
        st.matches.push(SearchMatch {
            path: rel.to_string(),
            line: u32::try_from(idx + 1).unwrap_or(u32::MAX),
            text: cut_line(line),
            context_before: before,
            context_after: after,
        });
        if st.matches.len() >= max_matches {
            return;
        }
    }
}

trait IdxSub {
    fn idx_sub(&self, a: usize, b: usize) -> &[&[u8]];
}
impl IdxSub for Vec<&[u8]> {
    fn idx_sub(&self, a: usize, b: usize) -> &[&[u8]] {
        &self[a.min(self.len())..b.min(self.len())]
    }
}

fn cut_line(line: &[u8]) -> String {
    let s = String::from_utf8_lossy(line);
    if s.chars().count() <= LINE_CHAR_CUT {
        s.into_owned()
    } else {
        s.chars().take(LINE_CHAR_CUT).collect()
    }
}

// ---------------------------------------------------------------------------
// .gitignore (in-root only) — minimal gitignore semantics
// ---------------------------------------------------------------------------

fn load_gitignore(dir_fd: RawFd, rel_prefix: &str, _st: &SearchState) -> Vec<IgnoreRule> {
    let name = match CString::new(".gitignore") {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let fd = match openat2(
        dir_fd,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        0,
    ) {
        Ok(fd) => fd,
        Err(_) => return Vec::new(),
    };
    let mut total = 0u64;
    let data = match read_capped(fd, 256 * 1024, &mut total) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut rules = Vec::new();
    for raw in String::from_utf8_lossy(&data).lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negate, pat) = if let Some(rest) = line.strip_prefix('!') {
            (true, rest)
        } else {
            (false, line)
        };
        let (pat, directory_only) = if let Some(rest) = pat.strip_suffix('/') {
            (rest, true)
        } else {
            (pat, false)
        };
        if pat.is_empty() || pat.len() > 512 {
            continue;
        }
        rules.push(IgnoreRule {
            base: rel_prefix.to_string(),
            pattern: pat.to_string(),
            negate,
            directory_only,
        });
    }
    rules
}

fn is_ignored(parent: &[IgnoreRule], own: &[IgnoreRule], rel: &str, is_dir: bool) -> bool {
    let mut drop = false;
    for rule in parent.iter().chain(own.iter()) {
        let target = rel
            .strip_prefix(&rule.base)
            .unwrap_or(rel)
            .trim_start_matches('/');
        if target.is_empty() || target.starts_with("..") || (rule.directory_only && !is_dir) {
            continue;
        }
        let hit = if rule.pattern.contains('/') {
            glob_match_path(&rule.pattern, target)
        } else {
            glob_match_path(&rule.pattern, basename(target))
                || glob_match_path(&rule.pattern, target)
        };
        if hit {
            drop = !rule.negate;
        }
    }
    drop
}

fn basename(rel: &str) -> &str {
    match rel.rsplit_once('/') {
        Some((_, n)) => n,
        None => rel,
    }
}
