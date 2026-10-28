//! Cross-process workspace coordinator backed by a lock directory.
//! Ported from `packages/toolpacks-coding/src/coordination-lockdir.ts`.
//!
//! Layout under `<dir>/<sha256(root)>/`:
//!   exclusive.lock           workspace-exclusive marker (command running)
//!   shared-<pid>-<uuid>.lock one per in-flight file lease
//!   file-<sha256(path)>.lock one per locked canonical file
//! Every lock file holds JSON `{pid, processStart, tool, label, createdAt}`
//! and is created atomically (written to a temp name, then `hard_link`ed into
//! place, which fails with EEXIST like O_EXCL but never exposes a half-written
//! file). `label` is only a 12-char hash of the holder's label: command text
//! never reaches the lock directory.
//!
//! A lock is stale when its pid is gone (`kill(pid, 0)` -> ESRCH) or the pid
//! now has a different start time (`ps -o lstart=`; pid reuse). A stale lock
//! is reclaimed by renaming it to a unique tombstone and re-checking the
//! tombstone's content before deleting it; if the tombstone turns out to hold
//! a live holder's lock, it is linked back (no overwrite) and treated as owned
//! by that holder.
//!
//! In-process fairness comes from the module-global in-process coordinator,
//! which is always acquired first, so packs using either kind of coordinator
//! in one process exclude each other.
//!
//! Limits: same host and same pid namespace only (pids and `ps` mean nothing
//! across machines or containers sharing the directory); if `ps` is
//! unavailable only pid liveness is checked; release failures are reported via
//! `on_error` and never change a tool result. An uncoordinated external editor
//! takes none of these locks; the tools' content checks detect its changes
//! instead.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::coordination::{
    AcquireFileInput, AcquireWorkspaceInput, CoordinationWait, CoordinatorObserver,
    InProcessCoordinator, LeaseHolder, ReleaseFn, WaitKind, WaitTracker, WorkspaceCoordinator,
    WorkspaceLease, abort_error, canonical_file_key, canonical_root_key,
    default_in_process_coordinator, spawn_release, with_wait_cap,
};
use crate::errors::CodingError;
use crate::fs_util::sha256;

/** An unparseable lock file this old is garbage, not a half-written lock. */
const CORRUPT_GRACE_MS: i64 = 5000;
const START_TTL_MS: u64 = 1000;
const DEFAULT_POLL_MS: u64 = 50;

/// Receives non-fatal cleanup errors; the message is what
/// `process.emitWarning` would have printed.
pub type OnError = Arc<dyn Fn(String) + Send + Sync>;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn io_error(error: &std::io::Error) -> CodingError {
    CodingError::new("io_error", format!("{error}"))
}

fn errno_is(error: &std::io::Error, code: i32) -> bool {
    error.raw_os_error() == Some(code)
}

fn report(on_error: &OnError, error: &dyn std::fmt::Display) {
    (*on_error)(format!("{error}"));
}

fn pid_alive(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true; // not a signalable pid: never declared dead
    };
    // SAFETY: kill(pid, 0) performs error checking only, sends no signal.
    unsafe {
        if libc::kill(pid, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

type StartCache = Mutex<HashMap<i64, (Option<String>, Instant)>>;

static STARTS: LazyLock<StartCache> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Process start time as printed by ps; None when it cannot be read.
async fn process_start(pid: i64) -> Option<String> {
    let own = pid == i64::from(std::process::id());
    if let Some((value, at)) = STARTS.lock().expect("starts cache").get(&pid)
        && (own || at.elapsed() < Duration::from_millis(START_TTL_MS))
    {
        return value.clone();
    }
    let value = tokio::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|text| !text.is_empty());
    STARTS
        .lock()
        .expect("starts cache")
        .insert(pid, (value.clone(), Instant::now()));
    value
}

#[derive(Clone)]
struct LockRecord {
    pid: i64,
    /// `ps -o lstart=` of the owner when it took the lock; absent if unknown.
    process_start: Option<serde_json::Value>,
    tool: String,
    /// sha256(label).slice(0, 12).
    label: String,
}

fn unknown_record() -> LockRecord {
    LockRecord {
        pid: -1,
        process_start: None,
        tool: "unknown".to_owned(),
        label: "unreadable lock file".to_owned(),
    }
}

async fn is_stale(record: &LockRecord) -> bool {
    if !pid_alive(record.pid) {
        return true;
    }
    match &record.process_start {
        None => false,
        Some(serde_json::Value::String(wanted)) => process_start(record.pid)
            .await
            .is_some_and(|now| now != *wanted),
        Some(_) => process_start(record.pid).await.is_some(),
    }
}

fn holder_of(record: &LockRecord) -> LeaseHolder {
    LeaseHolder {
        chat_id: None,
        run_id: format!("pid-{}", record.pid),
        tool_call_id: None,
        tool: record.tool.clone(),
        label: record.label.clone(),
    }
}

fn same_owner(a: &LockRecord, b: &LockRecord) -> bool {
    a.pid == b.pid && a.process_start == b.process_start
}

enum Read {
    Record(LockRecord),
    Gone,
    Corrupt,
}

async fn remove(path: &str) -> Result<(), CodingError> {
    match tokio::fs::remove_file(path).await {
        Err(error) if !errno_is(&error, libc::ENOENT) => Err(io_error(&error)),
        _ => Ok(()),
    }
}

async fn read_record(path: &str) -> Result<Read, CodingError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if errno_is(&error, libc::ENOENT) => return Ok(Read::Gone),
        Err(error) => return Err(io_error(&error)),
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(Read::Corrupt);
    };
    let Some(object) = parsed.as_object() else {
        return Ok(Read::Corrupt);
    };
    let (Some(pid), Some(tool)) = (
        object.get("pid").and_then(serde_json::Value::as_i64),
        object.get("tool").and_then(serde_json::Value::as_str),
    ) else {
        return Ok(Read::Corrupt);
    };
    Ok(Read::Record(LockRecord {
        pid,
        process_start: object
            .get("processStart")
            .cloned()
            .filter(|value| !value.is_null()),
        tool: tool.to_owned(),
        label: object
            .get("label")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }))
}

enum Judged {
    Record(LockRecord),
    Corrupt,
}

/// Take a stale (or corrupt) lock out of the way. Returns the live record when
/// the file turned out to belong to someone else after all.
async fn reclaim(
    path: &str,
    judged: &Judged,
    on_error: &OnError,
) -> Result<Option<LockRecord>, CodingError> {
    let tomb = format!("{path}.stale-{}", uuid::Uuid::new_v4());
    if let Err(error) = tokio::fs::rename(path, &tomb).await {
        if errno_is(&error, libc::ENOENT) {
            return Ok(None);
        }
        return Err(io_error(&error));
    }
    let moved = read_record(&tomb).await?;
    let unchanged = match (&moved, judged) {
        (Read::Gone, _) => true,
        (Read::Corrupt, Judged::Corrupt) => true,
        (Read::Record(moved), Judged::Record(judged)) => same_owner(moved, judged),
        _ => false,
    };
    if unchanged {
        if let Err(error) = remove(&tomb).await {
            report(on_error, &error);
        }
        return Ok(None);
    }
    // We displaced a different holder's lock. hard_link never overwrites, so
    // put it back if the name is still free; either way that holder owns the
    // lock.
    if let Err(error) = tokio::fs::hard_link(&tomb, path).await {
        report(on_error, &error);
    }
    if let Err(error) = remove(&tomb).await {
        report(on_error, &error);
    }
    Ok(Some(match moved {
        Read::Record(record) => record,
        Read::Corrupt | Read::Gone => unknown_record(),
    }))
}

/// The live owner of a lock file, or None (also after reclaiming a stale one).
async fn live_owner(path: &str, on_error: &OnError) -> Result<Option<LockRecord>, CodingError> {
    match read_record(path).await? {
        Read::Gone => Ok(None),
        Read::Corrupt => {
            let mtime = tokio::fs::metadata(path)
                .await
                .ok()
                .and_then(|info| info.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|age| age.as_millis() as i64);
            if let Some(mtime) = mtime
                && now_ms() - mtime < CORRUPT_GRACE_MS
            {
                return Ok(Some(unknown_record()));
            }
            reclaim(path, &Judged::Corrupt, on_error).await
        }
        Read::Record(record) => {
            if !is_stale(&record).await {
                return Ok(Some(record));
            }
            reclaim(path, &Judged::Record(record), on_error).await
        }
    }
}

async fn sleep(ms: u64, cancellation: &Option<CancellationToken>) -> Result<(), CodingError> {
    let Some(token) = cancellation else {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        return Ok(());
    };
    if token.is_cancelled() {
        return Err(abort_error());
    }
    tokio::select! {
        biased;
        () = token.cancelled() => Err(abort_error()),
        () = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
    }
}

/// Atomically create `path` with our record; false when it already exists.
async fn try_create(
    dir: &str,
    path: &str,
    holder: &LeaseHolder,
    on_error: &OnError,
) -> Result<bool, CodingError> {
    let pid = i64::from(std::process::id());
    let start = process_start(pid).await;
    let mut record = serde_json::Map::new();
    record.insert("pid".to_owned(), serde_json::Value::from(pid));
    if let Some(start) = &start {
        record.insert(
            "processStart".to_owned(),
            serde_json::Value::String(start.clone()),
        );
    }
    record.insert(
        "tool".to_owned(),
        serde_json::Value::String(holder.tool.clone()),
    );
    record.insert(
        "label".to_owned(),
        serde_json::Value::String(sha256(holder.label.as_bytes()).chars().take(12).collect()),
    );
    record.insert("createdAt".to_owned(), serde_json::Value::from(now_ms()));
    let temp = format!("{dir}/.tmp-{}", uuid::Uuid::new_v4());
    let written: Result<(), CodingError> = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(|error| io_error(&error))?;
        let text =
            serde_json::to_string(&serde_json::Value::Object(record)).expect("record serialises");
        file.write_all(text.as_bytes())
            .await
            .map_err(|error| io_error(&error))?;
        Ok(())
    }
    .await;
    written?;
    let result = match tokio::fs::hard_link(&temp, path).await {
        Ok(()) => Ok(true),
        Err(error) if errno_is(&error, libc::EEXIST) => Ok(false),
        Err(error) => Err(io_error(&error)),
    };
    // After a successful link the lock is ours; a failed unlink is only litter.
    if let Err(error) = remove(&temp).await {
        (*on_error)(format!("{error}"));
    }
    result
}

type Block<'a> = &'a (dyn Fn(WaitKind, &[LeaseHolder]) + Send + Sync);

struct LockDir {
    ws_dir: String,
    poll_ms: u64,
    on_error: OnError,
}

impl LockDir {
    fn marker(&self) -> String {
        format!("{}/exclusive.lock", self.ws_dir)
    }

    async fn acquire_shared(
        &self,
        holder: &LeaseHolder,
        cancellation: &Option<CancellationToken>,
        block: Block<'_>,
    ) -> Result<String, CodingError> {
        let entry = format!(
            "{}/shared-{}-{}.lock",
            self.ws_dir,
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        let marker = self.marker();
        loop {
            let first = live_owner(&marker, &self.on_error).await?;
            if let Some(first) = first {
                block(WaitKind::Workspace, &[holder_of(&first)]);
            } else {
                try_create(&self.ws_dir, &entry, holder, &self.on_error).await?;
                // A marker may have appeared between the check and our entry.
                let second = match live_owner(&marker, &self.on_error).await {
                    Ok(second) => second,
                    Err(error) => {
                        if let Err(error) = remove(&entry).await {
                            report(&self.on_error, &error);
                        }
                        return Err(error);
                    }
                };
                let Some(second) = second else {
                    return Ok(entry);
                };
                remove(&entry).await?;
                block(WaitKind::Workspace, &[holder_of(&second)]);
            }
            sleep(self.poll_ms, cancellation).await?;
        }
    }

    async fn acquire_exclusive(
        &self,
        holder: &LeaseHolder,
        cancellation: &Option<CancellationToken>,
        block: Block<'_>,
    ) -> Result<String, CodingError> {
        let marker = self.marker();
        while !try_create(&self.ws_dir, &marker, holder, &self.on_error).await? {
            let owner = live_owner(&marker, &self.on_error).await?;
            let Some(owner) = owner else { continue };
            block(WaitKind::Workspace, &[holder_of(&owner)]);
            sleep(self.poll_ms, cancellation).await?;
        }
        let waited: Result<(), CodingError> = async {
            loop {
                let live = self.live_shared().await?;
                if live.is_empty() {
                    return Ok(());
                }
                block(
                    WaitKind::Workspace,
                    &live.iter().map(holder_of).collect::<Vec<_>>(),
                );
                sleep(self.poll_ms, cancellation).await?;
            }
        }
        .await;
        if let Err(error) = waited {
            // A marker we cannot hand over must not linger either way.
            if let Err(error) = remove(&marker).await {
                report(&self.on_error, &error);
            }
            return Err(error);
        }
        Ok(marker)
    }

    async fn acquire_file_lock(
        &self,
        key: &str,
        holder: &LeaseHolder,
        cancellation: &Option<CancellationToken>,
        block: Block<'_>,
    ) -> Result<String, CodingError> {
        let path = format!("{}/file-{}.lock", self.ws_dir, sha256(key.as_bytes()));
        while !try_create(&self.ws_dir, &path, holder, &self.on_error).await? {
            let owner = live_owner(&path, &self.on_error).await?;
            let Some(owner) = owner else { continue };
            block(WaitKind::File, &[holder_of(&owner)]);
            sleep(self.poll_ms, cancellation).await?;
        }
        Ok(path)
    }

    async fn live_shared(&self) -> Result<Vec<LockRecord>, CodingError> {
        let mut names = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.ws_dir)
            .await
            .map_err(|error| io_error(&error))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| io_error(&error))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("shared-") && name.ends_with(".lock") {
                names.push(name);
            }
        }
        let mut live = Vec::new();
        for name in names {
            if let Some(owner) =
                live_owner(&format!("{}/{}", self.ws_dir, name), &self.on_error).await?
            {
                live.push(owner);
            }
        }
        Ok(live)
    }
}

/// Removes each path on its own and always frees the in-process gate.
fn lease_of(inner: WorkspaceLease, paths: Vec<String>, on_error: &OnError) -> WorkspaceLease {
    let mut releases: Vec<ReleaseFn> = Vec::new();
    for path in paths {
        let on_error = Arc::clone(on_error);
        releases.push(Box::new(move || {
            Box::pin(async move {
                if let Err(error) = remove(&path).await {
                    (*on_error)(format!("{error}"));
                }
            })
        }));
    }
    releases.push(Box::new(move || {
        let mut inner = inner;
        Box::pin(async move { inner.release().await })
    }));
    WorkspaceLease::new(releases)
}

fn default_on_error() -> OnError {
    Arc::new(|message| eprintln!("Warning: lock-dir coordinator: {message}"))
}

/// Lock files created by an acquisition that has not handed them to a lease
/// yet. If the acquisition future is dropped mid-flight (harness timeout or
/// cancellation), `Drop` deletes them — the same contract as
/// `WorkspaceLease::drop`; without it the orphan lock carries this live pid,
/// is never declared stale, and wedges the workspace for the process.
struct PendingLocks {
    paths: Vec<String>,
    on_error: OnError,
}

impl PendingLocks {
    fn new(on_error: &OnError) -> Self {
        Self {
            paths: Vec::new(),
            on_error: Arc::clone(on_error),
        }
    }

    fn push(&mut self, path: String) {
        self.paths.push(path);
    }

    fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.paths)
    }

    fn consume(mut self) -> Vec<String> {
        std::mem::take(&mut self.paths)
    }
}

impl Drop for PendingLocks {
    fn drop(&mut self) {
        for path in std::mem::take(&mut self.paths) {
            let on_error = Arc::clone(&self.on_error);
            spawn_release(Box::new(move || {
                Box::pin(async move {
                    if let Err(error) = remove(&path).await {
                        (*on_error)(format!("{error}"));
                    }
                })
            }));
        }
    }
}

async fn open(
    dir: &str,
    root: &str,
    poll_ms: u64,
    on_error: &OnError,
) -> Result<(String, LockDir), CodingError> {
    let key = canonical_root_key(root).await;
    let ws_dir = format!("{}/{}", dir, sha256(key.as_bytes()));
    tokio::fs::create_dir_all(&ws_dir)
        .await
        .map_err(|error| io_error(&error))?;
    Ok((
        key,
        LockDir {
            ws_dir,
            poll_ms,
            on_error: Arc::clone(on_error),
        },
    ))
}

struct LockDirCoordinator {
    dir: String,
    observer: Option<Arc<dyn CoordinatorObserver>>,
    poll_ms: u64,
    on_error: OnError,
    inner: Arc<InProcessCoordinator>,
}

#[async_trait]
impl WorkspaceCoordinator for LockDirCoordinator {
    async fn acquire_file(&self, input: AcquireFileInput) -> Result<WorkspaceLease, CodingError> {
        let tracker = WaitTracker::default();
        let observer = self.observer.clone();
        let inner = Arc::clone(&self.inner);
        let dir = self.dir.clone();
        let poll_ms = self.poll_ms;
        let on_error = Arc::clone(&self.on_error);
        let holder = input.holder.clone();
        let root = input.root.clone();
        let path = input.path.clone();
        let tracked = tracker.clone();
        let tracked_observer = observer.clone();
        let lease = with_wait_cap(
            input.cancellation.clone(),
            input.max_wait_ms,
            &tracker,
            move |cancellation| {
                let (inner, dir, on_error, holder, root, path) =
                    (inner, dir, on_error, holder, root, path);
                let (tracker, observer) = (tracked, tracked_observer);
                Box::pin(async move {
                    let held = inner
                        .acquire_file_tracked(&root, &path, &holder, &cancellation, &tracker)
                        .await?;
                    let mut locks = PendingLocks::new(&on_error);
                    let outcome: Result<(), CodingError> = async {
                        let (key, lock_dir) = open(&dir, &root, poll_ms, &on_error).await?;
                        let file_key = canonical_file_key(&path).await;
                        let block = |kind: WaitKind, holders: &[LeaseHolder]| {
                            tracker.waiting(
                                CoordinationWait {
                                    kind,
                                    root: key.clone(),
                                    path: (kind == WaitKind::File).then(|| file_key.clone()),
                                    waiter: holder.clone(),
                                    holders: holders.to_vec(),
                                },
                                observer.as_ref(),
                            );
                        };
                        locks.push(
                            lock_dir
                                .acquire_shared(&holder, &cancellation, &block)
                                .await?,
                        );
                        locks.push(
                            lock_dir
                                .acquire_file_lock(&file_key, &holder, &cancellation, &block)
                                .await?,
                        );
                        Ok(())
                    }
                    .await;
                    if let Err(error) = outcome {
                        lease_of(held, locks.take(), &on_error).release().await;
                        return Err(error);
                    }
                    Ok(lease_of(held, locks.consume(), &on_error))
                })
            },
        )
        .await?;
        tracker.acquired(observer.as_ref());
        Ok(lease)
    }

    async fn acquire_workspace(
        &self,
        input: AcquireWorkspaceInput,
    ) -> Result<WorkspaceLease, CodingError> {
        let tracker = WaitTracker::default();
        let observer = self.observer.clone();
        let inner = Arc::clone(&self.inner);
        let dir = self.dir.clone();
        let poll_ms = self.poll_ms;
        let on_error = Arc::clone(&self.on_error);
        let holder = input.holder.clone();
        let root = input.root.clone();
        let tracked = tracker.clone();
        let tracked_observer = observer.clone();
        let lease = with_wait_cap(
            input.cancellation.clone(),
            input.max_wait_ms,
            &tracker,
            move |cancellation| {
                let (inner, dir, on_error, holder, root) = (inner, dir, on_error, holder, root);
                let (tracker, observer) = (tracked, tracked_observer);
                Box::pin(async move {
                    let held = inner
                        .acquire_workspace_tracked(&root, &holder, &cancellation, &tracker)
                        .await?;
                    let mut locks = PendingLocks::new(&on_error);
                    let outcome: Result<(), CodingError> = async {
                        let (key, lock_dir) = open(&dir, &root, poll_ms, &on_error).await?;
                        let block = |kind: WaitKind, holders: &[LeaseHolder]| {
                            tracker.waiting(
                                CoordinationWait {
                                    kind,
                                    root: key.clone(),
                                    path: None,
                                    waiter: holder.clone(),
                                    holders: holders.to_vec(),
                                },
                                observer.as_ref(),
                            );
                        };
                        locks.push(
                            lock_dir
                                .acquire_exclusive(&holder, &cancellation, &block)
                                .await?,
                        );
                        Ok(())
                    }
                    .await;
                    if let Err(error) = outcome {
                        lease_of(held, locks.take(), &on_error).release().await;
                        return Err(error);
                    }
                    Ok(lease_of(held, locks.consume(), &on_error))
                })
            },
        )
        .await?;
        tracker.acquired(observer.as_ref());
        Ok(lease)
    }
}

/// Options for [`create_lockdir_workspace_coordinator`], mirroring the TS
/// options object `{ dir, observer?, pollMs?, onError? }`.
pub struct LockDirCoordinatorOptions {
    pub dir: String,
    pub observer: Option<Arc<dyn CoordinatorObserver>>,
    pub poll_ms: Option<u64>,
    /// Receives non-fatal cleanup errors. Default: warn on stderr.
    pub on_error: Option<OnError>,
}

pub fn create_lockdir_workspace_coordinator(
    options: LockDirCoordinatorOptions,
) -> Arc<dyn WorkspaceCoordinator> {
    Arc::new(LockDirCoordinator {
        dir: options.dir,
        observer: options.observer,
        poll_ms: options.poll_ms.unwrap_or(DEFAULT_POLL_MS),
        on_error: options.on_error.unwrap_or_else(default_on_error),
        inner: default_in_process_coordinator(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn temp_dir(name: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "ak-lockdir-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path.to_string_lossy().into_owned()
    }

    fn holder(id: &str, tool: &str, label: &str) -> LeaseHolder {
        LeaseHolder {
            chat_id: Some("chat".to_owned()),
            run_id: id.to_owned(),
            tool_call_id: None,
            tool: tool.to_owned(),
            label: label.to_owned(),
        }
    }

    fn coordinator(
        dir: &str,
        observer: Option<Arc<dyn CoordinatorObserver>>,
    ) -> (Arc<dyn WorkspaceCoordinator>, Arc<Mutex<Vec<String>>>) {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&errors);
        let created = create_lockdir_workspace_coordinator(LockDirCoordinatorOptions {
            dir: dir.to_owned(),
            observer,
            poll_ms: Some(20),
            on_error: Some(Arc::new(move |message| {
                sink.lock().expect("errors").push(message);
            })),
        });
        (created, errors)
    }

    async fn ws_dir(dir: &str, root: &str) -> String {
        format!(
            "{}/{}",
            dir,
            sha256(canonical_root_key(root).await.as_bytes())
        )
    }

    fn record(pid: i64, tool: &str, label: &str) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("pid".to_owned(), Value::from(pid));
        map.insert("tool".to_owned(), Value::from(tool));
        map.insert("label".to_owned(), Value::from(label));
        map.insert("createdAt".to_owned(), Value::from(now_ms()));
        map
    }

    async fn write_lock(dir: &str, root: &str, name: &str, body: &Value) -> String {
        let ws = ws_dir(dir, root).await;
        std::fs::create_dir_all(&ws).expect("create ws dir");
        let path = format!("{ws}/{name}");
        std::fs::write(&path, body.to_string()).expect("write lock");
        path
    }

    fn entries(dir: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read ws dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// A pid that is certainly gone: a killed child, reaped by wait.
    async fn dead_pid() -> i64 {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = i64::from(child.id().expect("pid"));
        let _ = child.kill().await;
        let _ = child.wait().await;
        pid
    }

    /// Inject the clock: make the file look `ms` milliseconds older.
    fn set_mtime_ago(path: &str, ms: i64) {
        let c_path = std::ffi::CString::new(path).expect("path");
        let stamp = libc::timespec {
            tv_sec: ((now_ms() - ms) / 1000) as libc::time_t,
            tv_nsec: 0,
        };
        let times = [stamp, stamp];
        // SAFETY: valid fd constant, C string and 2-element timespec array.
        unsafe {
            assert_eq!(
                libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0,),
                0,
                "utimensat"
            );
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        waits: Arc<Mutex<Vec<CoordinationWait>>>,
    }

    impl CoordinatorObserver for RecordingObserver {
        fn on_waiting(&self, wait: &CoordinationWait) {
            self.waits.lock().expect("waits").push(wait.clone());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_coordinators_in_one_process_exclude_each_other() {
        let dir = temp_dir("pair-dir");
        let root = temp_dir("pair-root");
        let file = format!("{root}/f.txt");
        std::fs::write(&file, "x").expect("write file");
        let (first, first_errors) = coordinator(&dir, None);
        let (second, second_errors) = coordinator(&dir, None);

        let held = first
            .acquire_file(AcquireFileInput {
                root: root.clone(),
                path: file.clone(),
                holder: holder("e1", "edit_file", "edit f"),
                cancellation: None,
                max_wait_ms: None,
            })
            .await
            .expect("first acquires");
        let ws = ws_dir(&dir, &root).await;
        let current = entries(&ws);
        assert_eq!(current.len(), 2, "{current:?}: shared entry + file lock");
        assert!(current[0].starts_with(&"file-".to_string()));
        assert!(current[1].starts_with(&format!("shared-{}-", std::process::id())));
        let body: Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{ws}/{}", current[1])).expect("read"),
        )
        .expect("parse shared record");
        assert_eq!(body["pid"], Value::from(i64::from(std::process::id())));
        assert_eq!(body["tool"], Value::from("edit_file"));
        assert_eq!(
            body["label"],
            Value::from(
                sha256("edit f".as_bytes())
                    .chars()
                    .take(12)
                    .collect::<String>()
            )
        );

        // The shared in-process gate excludes the second coordinator.
        let busy = second
            .acquire_file(AcquireFileInput {
                root: root.clone(),
                path: file.clone(),
                holder: holder("e2", "edit_file", "edit f"),
                cancellation: None,
                max_wait_ms: Some(100),
            })
            .await
            .expect_err("blocked by the first lease");
        assert_eq!(busy.code, "workspace_busy");
        assert_eq!(
            busy.message,
            "another session is running edit_file: edit f (waited 100ms). Try again later."
        );

        let mut held = held;
        held.release().await;
        let mut granted = second
            .acquire_file(AcquireFileInput {
                root,
                path: file,
                holder: holder("e2", "edit_file", "edit f"),
                cancellation: None,
                max_wait_ms: Some(2000),
            })
            .await
            .expect("runs after the release");
        granted.release().await;
        assert!(first_errors.lock().expect("errors").is_empty());
        assert!(second_errors.lock().expect("errors").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lock_left_by_a_dead_process_is_reclaimed() {
        let dir = temp_dir("dead-dir");
        let root = temp_dir("dead-root");
        let (created, errors) = coordinator(&dir, None);
        let dead = dead_pid().await;
        write_lock(
            &dir,
            &root,
            "exclusive.lock",
            &Value::Object(record(dead, "run_command", "deaddead1234")),
        )
        .await;
        let mut lease = tokio::time::timeout(
            Duration::from_secs(3),
            created.acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("cmd", "run_command", "run"),
                cancellation: None,
                max_wait_ms: None,
            }),
        )
        .await
        .expect("stale lock reclaimed")
        .expect("acquires");
        let ws = ws_dir(&dir, &root).await;
        assert_eq!(entries(&ws), vec!["exclusive.lock".to_owned()]);
        let body: Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{ws}/exclusive.lock")).expect("read"),
        )
        .expect("parse");
        assert_eq!(body["pid"], Value::from(i64::from(std::process::id())));
        lease.release().await;

        // Pid reuse: a live pid with a different start time is stale too.
        let mut reused = record(i64::from(std::process::id()), "run_command", "reused12345");
        reused.insert(
            "processStart".to_owned(),
            Value::from("Thu Jan  1 00:00:00 1970"),
        );
        write_lock(&dir, &root, "exclusive.lock", &Value::Object(reused)).await;
        let mut lease = tokio::time::timeout(
            Duration::from_secs(3),
            created.acquire_workspace(AcquireWorkspaceInput {
                root,
                holder: holder("cmd", "run_command", "run again"),
                cancellation: None,
                max_wait_ms: None,
            }),
        )
        .await
        .expect("reused pid reclaimed")
        .expect("acquires");
        lease.release().await;
        assert!(errors.lock().expect("errors").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_lock_dir_holder_produces_the_busy_message() {
        let dir = temp_dir("busy-dir");
        let root = temp_dir("busy-root");
        let observer = Arc::new(RecordingObserver::default());
        let waits = Arc::clone(&observer.waits);
        let (created, errors) = coordinator(&dir, Some(Arc::clone(&observer) as _));
        write_lock(
            &dir,
            &root,
            "exclusive.lock",
            &Value::Object(record(
                i64::from(std::process::id()),
                "run_command",
                "abc123def456",
            )),
        )
        .await;
        let busy = created
            .acquire_workspace(AcquireWorkspaceInput {
                root,
                holder: holder("cmd", "run_command", "run"),
                cancellation: None,
                max_wait_ms: Some(50),
            })
            .await
            .expect_err("a live exclusive lock blocks");
        assert_eq!(busy.code, "workspace_busy");
        assert_eq!(
            busy.message,
            "another session is running run_command: abc123def456 (waited 50ms). Try again later."
        );
        let waits = waits.lock().expect("waits");
        assert_eq!(waits.len(), 1, "observer notified once per request");
        assert_eq!(waits[0].kind, WaitKind::Workspace);
        assert_eq!(waits[0].holders[0].tool, "run_command");
        assert_eq!(
            waits[0].holders[0].run_id,
            format!("pid-{}", std::process::id())
        );
        assert!(errors.lock().expect("errors").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn release_is_idempotent() {
        let dir = temp_dir("release-dir");
        let root = temp_dir("release-root");
        let file = format!("{root}/f.txt");
        std::fs::write(&file, "x").expect("write file");
        let (created, errors) = coordinator(&dir, None);
        let input = AcquireFileInput {
            root: root.clone(),
            path: file.clone(),
            holder: holder("e", "edit_file", "edit f"),
            cancellation: None,
            max_wait_ms: None,
        };
        let mut lease = created.acquire_file(input.clone()).await.expect("acquires");
        lease.release().await;
        let ws = ws_dir(&dir, &root).await;
        assert!(entries(&ws).is_empty(), "lock files removed");
        lease.release().await; // second release: no-op
        created
            .acquire_file(input)
            .await
            .expect("gate freed after release")
            .release()
            .await;
        assert!(errors.lock().expect("errors").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abort_while_waiting_leaves_no_marker_behind() {
        let dir = temp_dir("abort-dir");
        let root = temp_dir("abort-root");
        let (created, errors) = coordinator(&dir, None);
        let marker = write_lock(
            &dir,
            &root,
            "exclusive.lock",
            &Value::Object(record(
                i64::from(std::process::id()),
                "run_command",
                "holdhold1234",
            )),
        )
        .await;
        let token = CancellationToken::new();
        let waiter = {
            let created = Arc::clone(&created);
            let root = root.clone();
            let token = token.clone();
            tokio::spawn(async move {
                created
                    .acquire_workspace(AcquireWorkspaceInput {
                        root,
                        holder: holder("cmd", "run_command", "run"),
                        cancellation: Some(token),
                        max_wait_ms: None,
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
        let aborted = waiter.await.expect("join").expect_err("aborts");
        assert_eq!(aborted.code, "aborted");
        assert_eq!(aborted.message, "the call was aborted");
        let ws = ws_dir(&dir, &root).await;
        assert_eq!(
            entries(&ws),
            vec!["exclusive.lock".to_owned()],
            "only the foreign marker remains"
        );
        std::fs::remove_file(&marker).expect("remove foreign marker");
        let mut lease = tokio::time::timeout(
            Duration::from_secs(2),
            created.acquire_workspace(AcquireWorkspaceInput {
                root,
                holder: holder("again", "run_command", "again"),
                cancellation: None,
                max_wait_ms: None,
            }),
        )
        .await
        .expect("no leftover blocks the next acquire")
        .expect("acquires");
        lease.release().await;
        assert!(errors.lock().expect("errors").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_lock_is_unknown_until_the_grace_expires() {
        let dir = temp_dir("corrupt-dir");
        let root = temp_dir("corrupt-root");
        let (created, errors) = coordinator(&dir, None);
        let marker = write_lock(&dir, &root, "exclusive.lock", &Value::from("not json {{{")).await;
        // Inside the grace window: a half-written lock, wait for it.
        let busy = created
            .acquire_workspace(AcquireWorkspaceInput {
                root: root.clone(),
                holder: holder("cmd", "run_command", "run"),
                cancellation: None,
                max_wait_ms: Some(50),
            })
            .await
            .expect_err("corrupt lock blocks");
        assert_eq!(busy.code, "workspace_busy");
        assert_eq!(
            busy.message,
            "another session is running unknown: unreadable lock file (waited 50ms). Try again later."
        );
        // Inject the clock: older than the grace, the file is garbage.
        set_mtime_ago(&marker, CORRUPT_GRACE_MS + 5000);
        let ws = ws_dir(&dir, &root).await;
        let mut lease = tokio::time::timeout(
            Duration::from_secs(3),
            created.acquire_workspace(AcquireWorkspaceInput {
                root,
                holder: holder("cmd", "run_command", "run"),
                cancellation: None,
                max_wait_ms: None,
            }),
        )
        .await
        .expect("corrupt lock reclaimed")
        .expect("acquires");
        assert_eq!(entries(&ws), vec!["exclusive.lock".to_owned()]);
        lease.release().await;
        assert!(errors.lock().expect("errors").is_empty());
    }
}
