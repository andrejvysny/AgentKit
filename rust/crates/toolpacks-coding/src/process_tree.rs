//! Find processes by the `AGENTKIT_EXEC_ID` marker every spawned command
//! carries in its environment. The marker is inherited by every descendant,
//! so it finds processes that left the process group (setsid, double fork).
//! A process that scrubs its own environment (`env -i`) escapes it; the group
//! kill is the only net for those.
//!
//! Ported from `packages/toolpacks-coding/src/process-tree.ts`.

use std::collections::{HashMap, HashSet};
use std::process::id as process_id;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use regex::Regex;
use tokio::sync::Mutex;

/// The environment variable every spawned command carries.
pub const EXEC_ID_ENV: &str = "AGENTKIT_EXEC_ID";

/// A process carrying an `AGENTKIT_EXEC_ID` marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkedProcess {
    pub pid: i32,
    pub exec_id: String,
    /// pid of the agent process that spawned the command; TS accepts any
    /// positive safe integer, so it is wider than a real `pid_t`.
    pub owner_pid: i64,
}

static ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]+):(\S+)$").expect("id pattern"));

// `ps -E` appends the environment to the command line, space separated.
// The TS lookahead `(?=\s|$)` is implied: `[0-9]+:\S+` is greedy and stops
// at whitespace or end-of-text anyway.
static PS_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?:^|\s){EXEC_ID_ENV}=([0-9]+:\S+)")).expect("ps token pattern")
});

static PS_HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([0-9]+)\s(.*)$").expect("ps head pattern"));

static PS_TABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*([0-9]+)\s+([0-9]+)\s+(\S.*\S)\s*$").expect("ps table pattern")
});

static NUMERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+$").expect("numeric pattern"));

/// `"<ownerPid>:<token>"` to its owner pid; None when malformed.
pub fn parse_exec_id(value: &str) -> Option<(i64, String)> {
    let caps = ID_PATTERN.captures(value)?;
    // TS accepts any positive safe integer (`Number.isSafeInteger`); reject
    // only `<= 0` and values above 2^53-1.
    let owner_pid: i64 = caps[1].parse::<i128>().ok()?.try_into().ok()?;
    if owner_pid <= 0 || owner_pid > (1i64 << 53) - 1 {
        return None;
    }
    Some((owner_pid, value.to_owned()))
}

/// Parse `ps -axwwE -o pid=,command=` output.
pub fn parse_ps_output(text: &str) -> Vec<MarkedProcess> {
    let mut found = Vec::new();
    for line in text.split('\n') {
        let Some(head) = PS_HEAD.captures(line) else {
            continue;
        };
        let Ok(pid) = head[1].parse::<i32>() else {
            continue;
        };
        let rest = head.get(2).map(|m| m.as_str()).unwrap_or("");
        let Some(token) = PS_TOKEN.captures(rest).map(|c| c[1].to_owned()) else {
            continue;
        };
        if let Some((owner_pid, exec_id)) = parse_exec_id(&token) {
            found.push(MarkedProcess {
                pid,
                exec_id,
                owner_pid,
            });
        }
    }
    found
}

/// Parse the raw contents of `/proc/<pid>/environ` (NUL separated).
pub fn parse_environ(raw: &str, pid: i32) -> Option<MarkedProcess> {
    let prefix = format!("{EXEC_ID_ENV}=");
    for entry in raw.split('\0') {
        if !entry.starts_with(&prefix) {
            continue;
        }
        let parsed = parse_exec_id(&entry[prefix.len()..])?;
        return Some(MarkedProcess {
            pid,
            exec_id: parsed.1,
            owner_pid: parsed.0,
        });
    }
    None
}

async fn run(file: &str, args: &[&str]) -> Result<String, std::io::Error> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(file).args(args).output(),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "ps timed out"))??;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn scan_proc() -> Result<Vec<MarkedProcess>, std::io::Error> {
    let mut found = Vec::new();
    let mut dir = tokio::fs::read_dir("/proc").await?;
    loop {
        // A transient iteration error (a pid vanishing under the cursor,
        // descriptor pressure under a loaded suite) must not turn the whole
        // scan into an unknown survivor set — that is a conservative block,
        // but it is not the truth. Retry briefly; a persistent failure still
        // returns Err, and the caller keeps treating it as unknown.
        let mut entry = None;
        for attempt in 0..3u32 {
            match dir.next_entry().await {
                Ok(next) => {
                    entry = next;
                    break;
                }
                Err(err) if attempt + 1 < 3 => {
                    let _ = err;
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(err) => return Err(err),
            }
        }
        let Some(entry) = entry else { break };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !NUMERIC.is_match(&name) {
            continue;
        }
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        // TS parity: an unreadable environ (kernel threads, processes of
        // other users on a shared machine — EPERM there is permanent, so
        // retrying would stall the whole scan) simply carries no marker.
        let raw = tokio::fs::read(format!("/proc/{name}/environ"))
            .await
            .unwrap_or_default();
        // latin1 in TS: every byte is one char; NUL splitting and the ASCII
        // prefix check are byte-faithful under a lossy decode.
        let raw = String::from_utf8_lossy(&raw);
        if let Some(mark) = parse_environ(&raw, pid) {
            found.push(mark);
        }
    }
    Ok(found)
}

/// Every marked process except this one; None when the platform cannot be
/// scanned or the scan failed. Never throws.
pub async fn scan_marked(platform: &str) -> Option<Vec<MarkedProcess>> {
    let all: Vec<MarkedProcess> = match platform {
        "darwin" => run("/bin/ps", &["-axwwE", "-o", "pid=,command="])
            .await
            .ok()
            .map(|text| parse_ps_output(&text))?,
        "linux" => {
            // /proc readdir is hash-ordered and unstable under churn: an
            // entry inserted after iteration passed its bucket can be
            // missed by a single pass. Missing a survivor is the dangerous
            // direction (a false "positively gone" retires an intent and
            // admits modifying work onto a live effect), so take the UNION
            // of two passes. Any one successful pass is enough to scan;
            // only when every pass fails is the platform unscannable.
            let first = scan_proc().await;
            let second = scan_proc().await;
            match (first, second) {
                (Ok(a), Ok(b)) => {
                    let mut a = a;
                    for mark in b {
                        if !a.iter().any(|seen| seen.pid == mark.pid) {
                            a.push(mark);
                        }
                    }
                    a
                }
                (Ok(a), Err(_)) | (Err(_), Ok(a)) => a,
                (Err(_), Err(_)) => return None,
            }
        }
        _ => return None,
    };
    let me = process_id() as i32;
    Some(all.into_iter().filter(|mark| mark.pid != me).collect())
}

/// Pids carrying exactly this exec id; None when unsupported.
pub async fn find_marked_processes(exec_id: &str) -> Option<Vec<i32>> {
    let all = scan_marked(crate_platform()).await;
    all.map(|all| {
        all.into_iter()
            .filter(|m| m.exec_id == exec_id)
            .map(|m| m.pid)
            .collect()
    })
}

/// The running OS, spelled like Node's `process.platform`.
pub(crate) fn crate_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

/// One row of `ps -axo pid=,ppid=,lstart=`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsRow {
    pub pid: i32,
    pub ppid: i32,
    /// `lstart`, which tells a recycled pid from the original process.
    pub started: String,
}

/// Parse `ps -axo pid=,ppid=,lstart=` output.
pub fn parse_ps_table(text: &str) -> Vec<PsRow> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        if let Some(m) = PS_TABLE.captures(line) {
            let (Ok(pid), Ok(ppid)) = (m[1].parse::<i32>(), m[2].parse::<i32>()) else {
                continue;
            };
            rows.push(PsRow {
                pid,
                ppid,
                started: m[3].to_owned(),
            });
        }
    }
    rows
}

/// Rows at or below any seed pid, following parent links.
pub fn descendants_of(rows: &[PsRow], seeds: impl IntoIterator<Item = i32>) -> Vec<PsRow> {
    let mut children: HashMap<i32, Vec<&PsRow>> = HashMap::new();
    for row in rows {
        children.entry(row.ppid).or_default().push(row);
    }
    let by_pid: HashMap<i32, &PsRow> = rows.iter().map(|row| (row.pid, row)).collect();
    let mut out: Vec<PsRow> = Vec::new();
    let mut seen: HashSet<i32> = HashSet::new();
    let mut queue: Vec<i32> = seeds.into_iter().collect();
    while let Some(pid) = queue.pop() {
        if let Some(self_row) = by_pid.get(&pid)
            && seen.insert(pid)
        {
            out.push((*self_row).clone());
        }
        for child in children.get(&pid).into_iter().flatten() {
            if !seen.contains(&child.pid) {
                seen.insert(child.pid);
                out.push((*child).clone());
                queue.push(child.pid);
            }
        }
    }
    out
}

/// macOS hides the environment of Apple-signed binaries (bash, sleep, perl),
/// so the marker cannot find what a shell script leaves behind. Instead the
/// process table is sampled while the command runs and every descendant of
/// the root is remembered; a double fork that reparents to launchd stays
/// known, unless it is created and orphaned between two samples.
pub struct TreeTracker {
    root_pid: i32,
    interval_ms: u64,
    max_interval_ms: u64,
    seen: Arc<Mutex<HashMap<i32, String>>>,
    stopped: Arc<AtomicBool>,
    root_gone: Arc<AtomicBool>,
}

impl TreeTracker {
    pub fn new(root_pid: i32, interval_ms: u64, max_interval_ms: u64) -> Self {
        Self {
            root_pid,
            interval_ms,
            max_interval_ms,
            seen: Arc::new(Mutex::new(HashMap::new())),
            stopped: Arc::new(AtomicBool::new(false)),
            root_gone: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Fast while a command is young (that is when shells fork and detach),
    /// then backing off: one `ps` costs tens of ms, and a 10-minute build
    /// sampled at 100 ms would spend a large share of a core on watching
    /// itself.
    pub fn start(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let seen = Arc::clone(&self.seen);
        let stopped = Arc::clone(&self.stopped);
        let root_gone = Arc::clone(&self.root_gone);
        let root_pid = self.root_pid;
        let interval_ms = self.interval_ms;
        let max_interval_ms = self.max_interval_ms;
        tokio::spawn(async move {
            let mut delay = interval_ms;
            loop {
                if stopped.load(Ordering::Relaxed) {
                    return;
                }
                sample_with(root_pid, &seen, &root_gone).await;
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                // ceil(delay * 1.25), capped, like the TS timer loop.
                delay = max_interval_ms.min((delay * 5).div_ceil(4));
            }
        });
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Sample now; the tracked pids that are still the same live processes.
    pub async fn sample(&self) -> Option<Vec<i32>> {
        sample_with(self.root_pid, &self.seen, &self.root_gone).await
    }
}

async fn sample_with(
    root_pid: i32,
    seen: &Mutex<HashMap<i32, String>>,
    root_gone: &AtomicBool,
) -> Option<Vec<i32>> {
    let text = match run("/bin/ps", &["-axo", "pid=,ppid=,lstart="]).await {
        Ok(text) => text,
        Err(_) => return None,
    };
    let rows = parse_ps_table(&text);
    let by_pid: HashMap<i32, &PsRow> = rows.iter().map(|row| (row.pid, row)).collect();
    let me = process_id() as i32;
    let mut seen_guard = seen.lock().await;
    seen_guard.retain(|pid, started| by_pid.get(pid).map(|row| &row.started) == Some(started));
    if !by_pid.contains_key(&root_pid) {
        root_gone.store(true, Ordering::Relaxed);
    }
    let seeds: Vec<i32> = if root_gone.load(Ordering::Relaxed) {
        seen_guard.keys().copied().collect()
    } else {
        let mut seeds = vec![root_pid];
        seeds.extend(seen_guard.keys().copied());
        seeds
    };
    for row in descendants_of(&rows, seeds) {
        if row.pid != me {
            seen_guard.insert(row.pid, row.started);
        }
    }
    Some(seen_guard.keys().copied().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exec_id_accepts_only_owner_token() {
        assert_eq!(
            parse_exec_id("123:abc-1"),
            Some((123, "123:abc-1".to_owned()))
        );
        assert_eq!(parse_exec_id("abc:1"), None);
        assert_eq!(parse_exec_id("0:x"), None);
        assert_eq!(parse_exec_id("12"), None);
        // TS accepts any positive SAFE integer: 3e9 must survive the crash
        // sweep (owner not alive -> marked process is terminated), while
        // values above 2^53-1 are rejected like `Number.isSafeInteger`.
        assert_eq!(
            parse_exec_id("3000000000:tok"),
            Some((3_000_000_000, "3000000000:tok".to_owned()))
        );
        assert_eq!(
            parse_exec_id("9007199254740991:x"),
            Some((9_007_199_254_740_991, "9007199254740991:x".to_owned()))
        );
        assert_eq!(parse_exec_id("9007199254740992:x"), None);
        assert_eq!(parse_exec_id("99999999999999999999:x"), None);
    }

    #[test]
    fn parse_ps_output_finds_the_marker_token_among_args_and_env() {
        let out = [
            "  101 /bin/sleep 300 HOME=/x AGENTKIT_EXEC_ID=42:u-1 TERM=dumb",
            "  102 /bin/ls AGENTKIT_EXEC_ID=bad",
            "  103 grep xAGENTKIT_EXEC_ID=7:z",
            "  104 /usr/bin/true AGENTKIT_EXEC_ID=9:k",
            "garbage",
        ]
        .join("\n");
        assert_eq!(
            parse_ps_output(&out),
            vec![
                MarkedProcess {
                    pid: 101,
                    exec_id: "42:u-1".to_owned(),
                    owner_pid: 42
                },
                MarkedProcess {
                    pid: 104,
                    exec_id: "9:k".to_owned(),
                    owner_pid: 9
                },
            ]
        );
    }

    #[test]
    fn parse_ps_table_and_descendants_follow_parent_links() {
        let rows = parse_ps_table(
            &[
                "    1     0 Mon Oct 27 08:00:00 2026",
                "  100     1 Mon Oct 27 09:00:00 2026",
                "  101   100 Mon Oct 27 09:00:01 2026",
                "  102   101 Mon Oct 27 09:00:02 2026",
                "  200     1 Mon Oct 27 09:00:03 2026",
                "junk",
            ]
            .join("\n"),
        );
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[2],
            PsRow {
                pid: 101,
                ppid: 100,
                started: "Mon Oct 27 09:00:01 2026".to_owned()
            }
        );
        let mut pids: Vec<i32> = descendants_of(&rows, [100])
            .into_iter()
            .map(|r| r.pid)
            .collect();
        pids.sort();
        assert_eq!(pids, vec![100, 101, 102]);
    }

    #[test]
    fn parse_environ_reads_a_nul_separated_environment() {
        let raw = "A=1\0AGENTKIT_EXEC_ID=5:abc\0B=2\0";
        assert_eq!(
            parse_environ(raw, 77),
            Some(MarkedProcess {
                pid: 77,
                exec_id: "5:abc".to_owned(),
                owner_pid: 5
            })
        );
        assert_eq!(parse_environ("A=1\0", 77), None);
        assert_eq!(parse_environ("AGENTKIT_EXEC_ID=nope\0", 77), None);
    }

    #[test]
    fn backoff_matches_ts_ceil_sequence() {
        // delay = min(max, ceil(delay * 1.25)) from 100:
        let mut delay = 100u64;
        let mut steps = vec![delay];
        for _ in 0..4 {
            delay = (delay * 5).div_ceil(4);
            steps.push(delay);
        }
        assert_eq!(steps, vec![100, 125, 157, 197, 247]);
    }
}
