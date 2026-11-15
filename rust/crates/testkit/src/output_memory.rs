//! In-memory `OutputStore` mirroring the SQLite adapter (the reference):
//! the same handle shape, the same index facts (length, line count, SHA-256,
//! created, expires), the same retention rule (activity on a session moves
//! all its expiries forward; an expired output reads as not found), the same
//! purge report. Bytes are held in the row instead of a private file.
use agentkit_core::{
    ByteOrLineRange, Match, OUTPUT_RETENTION_MS, OutputError, OutputHandle, OutputInfo, OutputMeta,
    OutputSlice, OutputStore, PurgeReport, Runtime, SessionTree, count_lines, search_bytes,
    slice_bytes,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A clock the test moves by hand.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicI64,
    ids: AtomicU64,
}

impl ManualClock {
    pub fn new(now_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            now: AtomicI64::new(now_ms),
            ids: AtomicU64::new(0),
        })
    }

    pub fn set(&self, now_ms: i64) {
        self.now.store(now_ms, Ordering::SeqCst);
    }

    pub fn advance(&self, by_ms: i64) {
        self.now.fetch_add(by_ms, Ordering::SeqCst);
    }
}

impl Runtime for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }

    fn id(&self) -> String {
        format!("manual-{}", self.ids.fetch_add(1, Ordering::SeqCst))
    }
}

#[derive(Default)]
struct State {
    rows: HashMap<OutputHandle, (OutputInfo, Vec<u8>)>,
    /// child session -> parent session (the `subagents` table's shape).
    parents: HashMap<String, String>,
}

#[derive(Clone)]
pub struct InMemoryOutputStore {
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Runtime>,
}

impl InMemoryOutputStore {
    pub fn new(clock: Arc<dyn Runtime>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            clock,
        }
    }

    /// Register `child` as a subagent session of `parent`.
    pub fn link_child(&self, child: &str, parent: &str) {
        self.lock().parents.insert(child.into(), parent.into());
    }

    /// Outputs held for `session`.
    pub fn stored(&self, session: &str) -> usize {
        self.lock()
            .rows
            .values()
            .filter(|(info, _)| info.meta.session == session)
            .count()
    }

    /// Delete every output whose retention ended.
    pub fn purge_expired(&self) -> PurgeReport {
        let now = self.clock.now_ms();
        let mut state = self.lock();
        let mut report = PurgeReport::default();
        state.rows.retain(|_, (info, _)| {
            let due = info.expires_ms <= now;
            if due {
                report.outputs += 1;
                report.bytes += info.byte_len;
            }
            !due
        });
        report
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The live bytes of a handle, moving its session's expiry forward.
    fn live(
        &self,
        handle: &OutputHandle,
        touch: bool,
    ) -> Result<(OutputInfo, Vec<u8>), OutputError> {
        let now = self.clock.now_ms();
        let mut state = self.lock();
        let Some((info, bytes)) = state.rows.get(handle).cloned() else {
            return Err(OutputError::NotFound(handle.to_string()));
        };
        if info.expires_ms <= now {
            return Err(OutputError::NotFound(handle.to_string()));
        }
        if touch {
            let expires = now.saturating_add(OUTPUT_RETENTION_MS);
            for (other, _) in state.rows.values_mut() {
                if other.meta.session == info.meta.session && other.expires_ms < expires {
                    other.expires_ms = expires;
                }
            }
        }
        Ok((info, bytes))
    }
}

#[async_trait]
impl OutputStore for InMemoryOutputStore {
    async fn put(&self, meta: OutputMeta, bytes: Vec<u8>) -> Result<OutputHandle, OutputError> {
        let now = self.clock.now_ms();
        let info = OutputInfo {
            handle: OutputHandle::generate(),
            byte_len: bytes.len() as u64,
            line_count: count_lines(&bytes),
            sha256: agentkit_kernel::sha256_hex(&bytes),
            created_ms: now,
            expires_ms: now.saturating_add(OUTPUT_RETENTION_MS),
            meta,
        };
        let handle = info.handle.clone();
        let session = info.meta.session.clone();
        self.lock().rows.insert(handle.clone(), (info, bytes));
        // Same as the adapter: a write is activity on the session.
        let expires = now.saturating_add(OUTPUT_RETENTION_MS);
        for (other, _) in self.lock().rows.values_mut() {
            if other.meta.session == session && other.expires_ms < expires {
                other.expires_ms = expires;
            }
        }
        Ok(handle)
    }

    async fn info(&self, handle: &OutputHandle) -> Result<OutputInfo, OutputError> {
        self.live(handle, false).map(|(info, _)| info)
    }

    async fn read(
        &self,
        handle: &OutputHandle,
        range: ByteOrLineRange,
    ) -> Result<OutputSlice, OutputError> {
        let (_, bytes) = self.live(handle, true)?;
        Ok(slice_bytes(&bytes, range))
    }

    async fn search(
        &self,
        handle: &OutputHandle,
        pattern: &str,
        max_matches: u32,
    ) -> Result<Vec<Match>, OutputError> {
        let (_, bytes) = self.live(handle, true)?;
        search_bytes(&bytes, pattern, max_matches)
    }

    async fn purge_session(&self, session: &str) -> Result<PurgeReport, OutputError> {
        let mut report = PurgeReport::default();
        self.lock().rows.retain(|_, (info, _)| {
            let ours = info.meta.session == session;
            if ours {
                report.outputs += 1;
                report.bytes += info.byte_len;
            }
            !ours
        });
        Ok(report)
    }
}

#[async_trait]
impl SessionTree for InMemoryOutputStore {
    async fn may_read(&self, caller: &str, owner: &str) -> bool {
        if caller == owner {
            return true;
        }
        let state = self.lock();
        let mut current = owner.to_owned();
        for _ in 0..32 {
            match state.parents.get(&current) {
                Some(parent) if parent == caller => return true,
                Some(parent) => current = parent.clone(),
                None => break,
            }
        }
        false
    }
}
