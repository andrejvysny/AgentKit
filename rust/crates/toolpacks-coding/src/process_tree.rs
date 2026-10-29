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

async fn read_status_euid(pid: &str) -> Option<u32> {
    let status = tokio::fs::read_to_string(format!("/proc/{pid}/status"))
        .await
        .ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // `Uid:\treal\teffective\tsaved\tfs` — the effective uid decides
            // who the kernel lets read a process's environ.
            return rest.split_whitespace().nth(1)?.parse().ok();
        }
    }
    None
}

/// Is this pid a zombie (`Z` in `/proc/<pid>/stat`)? Reading a zombie's
/// environ answers EPERM, so without this check an exited process still
/// waiting for its reaper would be classified as an unreadable-environ
/// SUSPECT — phantom uncertainty over a process that is provably dead and
/// can neither write nor hide a marker. An unreadable stat is NOT a zombie
/// (keep the suspect; fail closed).
async fn is_zombie(pid: &str) -> bool {
    let Ok(stat) = tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await else {
        return false;
    };
    // `pid (comm) state ...` — comm may contain spaces and parens; the
    // state is the first token after the LAST `)`.
    match stat.rfind(')') {
        Some(tail) => stat[tail + 1..]
            .split_whitespace()
            .next()
            .is_some_and(|state| state == "Z"),
        None => false,
    }
}

/// Process age in ms, measured purely against the monotonic boot clock:
/// `uptime - starttime`. Returns None when either side is unreadable.
async fn process_age_ms(pid: &str) -> Option<u64> {
    let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
        .await
        .ok()?;
    // `pid (comm) state ...` — comm may contain spaces and parens; count
    // fields from after the LAST `)`. `starttime` is field 22 overall, i.e.
    // the 20th token after comm (fields 3..=22 → 20 tokens).
    let tail = stat.rfind(')')?;
    let fields: Vec<&str> = stat[tail + 1..].split_whitespace().collect();
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    let uptime = tokio::fs::read_to_string("/proc/uptime").await.ok()?;
    let uptime_ms = uptime.split_whitespace().next()?.parse::<f64>().ok()? * 1000.0;
    let started_ms = ticks * 10; // USER_HZ = 100; misestimates fail closed.
    u64::try_from(uptime_ms as i64 - started_ms as i64).ok()
}

/// Can a process whose environ returned a permission error still be ours?
/// Three filters, all conservative (any doubt counts as ours):
/// 1. same effective user — a different-user process cannot be a child we
///    spawned without privileges we do not have;
/// 2. young enough to have been spawned by the execution in question —
///    long-lived same-user daemons that set `dumpable=0` (gpg-agent class)
///    are permanently unreadable and must not block every workspace
///    forever, but they also predate any execution we are reconciling;
/// 3. an unmeasurable start time stays suspect.
///
/// Treating a live marked child as "unmarked" is the dangerous direction
/// (a false "positively gone" admits modifying work onto a live effect),
/// so anything the filters cannot exclude — including a start time that
/// appears just before the spawn floor (clock-step skew) beyond slack —
/// counts as plausibly ours.
fn denied_environ_plausibly_ours(
    mine: Option<u32>,
    peer: Option<u32>,
    age_ms: Option<u64>,
    spawn_floor_ms_ago: Option<u64>,
) -> bool {
    match (mine, peer) {
        (Some(mine), Some(peer)) if mine != peer => return false,
        _ => {}
    }
    match (age_ms, spawn_floor_ms_ago) {
        // A process older than the spawn (beyond skew slack) predates the
        // execution and cannot carry its nonce.
        (Some(age), Some(floor)) => age <= floor.saturating_add(SUSPECT_SPAWN_SLACK_MS),
        _ => true,
    }
}

/// Slack (ms) absorbing clock skew between the recorded spawn wall time
/// and the monotonic process age before a suspect is excluded as too old.
const SUSPECT_SPAWN_SLACK_MS: u64 = 60_000;

struct ProcScan {
    marks: Vec<MarkedProcess>,
    /// Live pids whose environ was unreadable and which the ownership and
    /// age checks could not exclude as foreign: marked survivors may hide
    /// in there.
    suspects: Vec<i32>,
    /// Pids whose environment was actually readable. Absence from a pass
    /// is not a successful classification: the process may have appeared
    /// after that pass visited its /proc bucket.
    classified: HashSet<i32>,
}

async fn scan_proc(spawn_floor_ms_ago: Option<u64>) -> Result<ProcScan, std::io::Error> {
    let my_euid = read_status_euid("self").await;
    let mut found = Vec::new();
    let mut suspects = Vec::new();
    let mut classified = HashSet::new();
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
        match tokio::fs::read(format!("/proc/{name}/environ")).await {
            Ok(raw) => {
                classified.insert(pid);
                // latin1 in TS: every byte is one char; NUL splitting and the ASCII
                // prefix check are byte-faithful under a lossy decode.
                let raw = String::from_utf8_lossy(&raw);
                if let Some(mark) = parse_environ(&raw, pid) {
                    found.push(mark);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Vanished mid-scan: genuinely absent.
            }
            Err(_) => {
                // EPERM on another user's daemon is permanent, so it must not
                // stall or block every workspace; but an unreadable environ on
                // a same-user process young enough to be our spawn keeps a
                // marked survivor possible, and that uncertainty must survive
                // to the cleanup decision. A zombie (whose environ also
                // answers EPERM) is provably dead — never a suspect.
                if !is_zombie(&name).await
                    && denied_environ_plausibly_ours(
                        my_euid,
                        read_status_euid(&name).await,
                        process_age_ms(&name).await,
                        spawn_floor_ms_ago,
                    )
                {
                    suspects.push(pid);
                }
            }
        }
    }
    Ok(ProcScan {
        marks: found,
        suspects,
        classified,
    })
}

/// Combine two successful scans without treating a missing directory entry
/// as evidence of a readable environment. Either pass can discover a new
/// suspect; only an actual successful read by the other pass can resolve it.
fn merge_proc_scans(a: ProcScan, b: ProcScan) -> (Vec<MarkedProcess>, Vec<i32>) {
    let mut suspects = Vec::new();
    for pid in a.suspects {
        if !b.classified.contains(&pid) && !suspects.contains(&pid) {
            suspects.push(pid);
        }
    }
    for pid in b.suspects {
        if !a.classified.contains(&pid) && !suspects.contains(&pid) {
            suspects.push(pid);
        }
    }
    let mut marks = a.marks;
    for mark in b.marks {
        if !marks.iter().any(|seen| seen.pid == mark.pid) {
            marks.push(mark);
        }
    }
    suspects.retain(|pid| !marks.iter().any(|mark| mark.pid == *pid));
    (marks, suspects)
}

/// The marked-scan result with its completeness evidence: `marks` are the
/// processes positively carrying a marker, `suspect_pids` are live processes
/// whose environ could not be read and which the ownership and age checks
/// could not exclude as belonging to the execution being reconciled. A
/// cleanup decision may only treat the survivor set as settled when the
/// scan produced `Some` AND `suspect_pids` is empty — otherwise a live
/// marked child hiding behind an unreadable environ could be retired as
/// "positively gone". `spawned_at_ms` (the execution's recorded spawn time)
/// bounds which unreadable same-user processes can plausibly carry the
/// nonce; `None` (no spawn anchor) keeps every same-user unreadable process
/// suspect — maximum caution.
pub struct MarkedScan {
    pub marks: Vec<MarkedProcess>,
    pub suspect_pids: Vec<i32>,
}

/// Every marked process except this one, with uncertainty evidence; None
/// when the platform cannot be scanned or the scan failed. Never throws.
///
/// Darwin note: `ps -axwwE` only exposes the environment of same-user
/// processes and says nothing about the rest; the darwin suspect set is
/// therefore always empty, and the darwin scan keeps its pre-existing
/// completeness caveat (recorded in the binding/qualification docs).
pub async fn scan_marked_outcome(platform: &str, spawned_at_ms: Option<u64>) -> Option<MarkedScan> {
    let spawn_floor_ms_ago = spawned_at_ms.map(|spawned| {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or(u64::MAX);
        now_ms.saturating_sub(spawned)
    });
    let (all, suspects): (Vec<MarkedProcess>, Vec<i32>) = match platform {
        "darwin" => {
            let all = run("/bin/ps", &["-axwwE", "-o", "pid=,command="])
                .await
                .ok()
                .map(|text| parse_ps_output(&text))?;
            (all, Vec::new())
        }
        "linux" => {
            // /proc readdir is hash-ordered and unstable under churn: an
            // entry inserted after iteration passed its bucket can be missed
            // by a single pass. Missing a survivor is the dangerous direction
            // (a false "positively gone" retires an intent and admits
            // modifying work onto a live effect), so take the UNION of two
            // passes. Any one successful pass is enough to scan; only when
            // every pass fails is the platform unscannable. Suspects are
            // resolved only by an actual readable environment in the other
            // pass; absence from that pass is not positive classification.
            let first = scan_proc(spawn_floor_ms_ago).await;
            let second = scan_proc(spawn_floor_ms_ago).await;
            match (first, second) {
                (Ok(a), Ok(b)) => merge_proc_scans(a, b),
                (Ok(a), Err(_)) | (Err(_), Ok(a)) => (a.marks, a.suspects),
                (Err(_), Err(_)) => return None,
            }
        }
        _ => return None,
    };
    let me = process_id() as i32;
    Some(MarkedScan {
        marks: all.into_iter().filter(|mark| mark.pid != me).collect(),
        suspect_pids: suspects.into_iter().filter(|pid| *pid != me).collect(),
    })
}

/// Pids carrying exactly this exec id plus the unresolved-uncertainty pids;
/// None when unsupported. Pass the execution's `spawned_at_ms` so
/// long-lived unreadable daemons are excluded from the suspect set while
/// processes young enough to be the execution's own children are not.
pub async fn find_marked_processes_outcome(
    exec_id: &str,
    spawned_at_ms: Option<u64>,
) -> Option<(Vec<i32>, Vec<i32>)> {
    let scan = scan_marked_outcome(crate_platform(), spawned_at_ms).await?;
    Some((
        scan.marks
            .into_iter()
            .filter(|m| m.exec_id == exec_id)
            .map(|m| m.pid)
            .collect(),
        scan.suspect_pids,
    ))
}

/// Pids carrying exactly this exec id; None when unsupported. LOSSY:
/// drops the suspect set, so it must NEVER decide cleanup or
/// survivor-set emptiness (use `find_marked_processes_outcome`); no
/// production caller — tests only, where it merely POLLS for a marked
/// pid's presence (existence assertions cannot fabricate a "clean"
/// verdict). Kill policy: classifying a pid here says nothing about
/// killing it; suspects are never killed, only reported.
pub async fn find_marked_processes(exec_id: &str) -> Option<Vec<i32>> {
    find_marked_processes_outcome(exec_id, None)
        .await
        .map(|(pids, _)| pids)
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

    fn scan_with(suspects: Vec<i32>, classified: &[i32]) -> ProcScan {
        ProcScan {
            marks: vec![],
            suspects,
            classified: classified.iter().copied().collect(),
        }
    }

    #[test]
    fn scans_retain_a_suspect_absent_from_the_other_pass() {
        let (_, later) = merge_proc_scans(scan_with(vec![], &[]), scan_with(vec![42], &[]));
        assert_eq!(later, vec![42], "a newly appeared suspect is unresolved");
        let (_, earlier) = merge_proc_scans(scan_with(vec![42], &[]), scan_with(vec![], &[]));
        assert_eq!(
            earlier,
            vec![42],
            "a missing entry is not a readable environ"
        );
    }

    #[test]
    fn scans_resolve_a_suspect_only_after_a_successful_classification() {
        let (_, later_read) = merge_proc_scans(scan_with(vec![42], &[]), scan_with(vec![], &[42]));
        assert!(
            later_read.is_empty(),
            "a readable unrelated process may clear"
        );
        let (_, earlier_read) =
            merge_proc_scans(scan_with(vec![], &[42]), scan_with(vec![42], &[]));
        assert!(earlier_read.is_empty());

        let mut marked = scan_with(vec![], &[42]);
        marked.marks.push(MarkedProcess {
            pid: 42,
            exec_id: "123:nonce".into(),
            owner_pid: 123,
        });
        let (marks, suspects) = merge_proc_scans(scan_with(vec![42], &[]), marked);
        assert_eq!(marks.len(), 1, "positive marker classification is retained");
        assert!(suspects.is_empty());
    }

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

    #[test]
    fn denied_environ_uncertainty_is_ownership_aware() {
        // Same effective user and young enough: the denial cannot exclude
        // our own marked child, so the scan must stay uncertain.
        assert!(denied_environ_plausibly_ours(
            Some(1000),
            Some(1000),
            Some(0),
            Some(10_000)
        ));
        // A different effective user cannot be a process we spawned.
        assert!(!denied_environ_plausibly_ours(
            Some(1000),
            Some(0),
            Some(0),
            Some(10_000)
        ));
        // A same-user process older than the execution's spawn (beyond the
        // skew slack) predates it — a long-lived dumpable=0 daemon (the
        // gpg-agent class) must not block unrelated workspaces.
        assert!(!denied_environ_plausibly_ours(
            Some(1000),
            Some(1000),
            Some(500_000),
            Some(10_000)
        ));
        // Right at the slack boundary it still counts as plausibly ours.
        assert!(denied_environ_plausibly_ours(
            Some(1000),
            Some(1000),
            Some(10_000 + SUSPECT_SPAWN_SLACK_MS),
            Some(10_000)
        ));
        // An unmeasurable age cannot exclude the process.
        assert!(denied_environ_plausibly_ours(
            Some(1000),
            Some(1000),
            None,
            Some(10_000)
        ));
        // Without a recorded spawn time there is no window: stay uncertain.
        assert!(denied_environ_plausibly_ours(
            Some(1000),
            Some(1000),
            Some(500_000),
            None
        ));
        // Anything the comparison cannot rule out counts as ours.
        assert!(denied_environ_plausibly_ours(
            Some(1000),
            None,
            Some(0),
            Some(10_000)
        ));
        assert!(denied_environ_plausibly_ours(
            None,
            Some(0),
            Some(0),
            Some(10_000)
        ));
        assert!(denied_environ_plausibly_ours(None, None, None, None));
    }

    /// Reads `/proc` directly: Linux-only.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn own_effective_uid_is_readable_from_proc() {
        let euid = read_status_euid("self").await.expect("own status");
        assert_eq!(
            euid,
            read_status_euid(&std::process::id().to_string())
                .await
                .expect("own status by pid")
        );
    }

    /// Reads `/proc` directly: Linux-only.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn live_system_scan_stays_complete_without_foreign_suspects() {
        // A real machine runs other users' (and kernel) processes whose
        // environ is EPERM to us; none of them may turn into suspects —
        // unrelated unreadability must not block every workspace. Our own
        // processes must not be suspects either (same-user EPERM would be
        // a genuine uncertainty worth blocking on, and never arises for
        // ordinary own processes).
        let scan = scan_marked_outcome("linux", None)
            .await
            .expect("linux scans");
        let mine = read_status_euid("self").await.expect("own euid");
        for pid in scan.suspect_pids {
            let peer = read_status_euid(&pid.to_string())
                .await
                .unwrap_or_else(|| panic!("suspect {pid} lost mid-test"));
            assert_eq!(peer, mine, "foreign process {pid} became a suspect");
        }
    }
}
