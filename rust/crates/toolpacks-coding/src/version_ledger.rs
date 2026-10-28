//! The version of each file a session last read or wrote, so a later mutation
//! can refuse to overwrite changes the session never saw. Mirrors
//! `packages/toolpacks-coding/src/version-ledger.ts`.
//!
//! Process-global (like the coordinator) so every pack in the process, and
//! every run of one chat, shares one view. Bounded with LRU eviction; an
//! evicted entry just means "unknown" (write_file then asks for a re-read,
//! edit_file falls back to its under-lock content checks). Without a chatId
//! nothing is recorded.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

const MAX_PATHS_PER_CHAT: usize = 4096;
const MAX_CHATS: usize = 256;

/// `full` is true when the session saw the WHOLE file at this version (a
/// complete read, or its own write). A partial read (offset, limit, or output
/// truncation) records `full: false`: it may anchor an edit_file, whose edits
/// are matched against oldText, but never authorises a full overwrite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    pub sha256: String,
    pub full: bool,
}

#[derive(Clone)]
struct PathEntry {
    sha256: String,
    full: bool,
    /// Recency stamp; the lowest stamp in a chat is its least-recently-used
    /// path (a monotonically increasing tick reproduces JS Map iteration
    /// order, where delete+set moves an entry to the back).
    used: u64,
}

struct Chat {
    paths: HashMap<String, PathEntry>,
    used: u64,
}

#[derive(Default)]
struct Ledger {
    chats: HashMap<String, Chat>,
    clock: u64,
}

impl Ledger {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

static LEDGER: LazyLock<Mutex<Ledger>> = LazyLock::new(|| Mutex::new(Ledger::default()));

/// Insert-or-touch a chat, evicting the least recently used chats above the
/// bound. Returns the chat for further mutation.
fn touch_chat<'a>(state: &'a mut Ledger, chat_id: &str) -> &'a mut Chat {
    let used = state.tick();
    let chat = state.chats.entry(chat_id.to_owned()).or_insert(Chat {
        paths: HashMap::new(),
        used: 0,
    });
    chat.used = used;
    while state.chats.len() > MAX_CHATS {
        let oldest_used = state
            .chats
            .values()
            .map(|c| c.used)
            .min()
            .unwrap_or(u64::MAX);
        let victim = state
            .chats
            .iter()
            .find(|(_, c)| c.used == oldest_used)
            .map(|(k, _)| k.clone());
        match victim {
            Some(key) => {
                state.chats.remove(&key);
            }
            None => break,
        }
    }
    state
        .chats
        .get_mut(chat_id)
        .expect("chat present after touch/evict")
}

fn put(chat_id: Option<&str>, path: &str, sha256: &str, full: bool) {
    let Some(chat_id) = chat_id else {
        return;
    };
    let mut state = LEDGER.lock().expect("version ledger lock poisoned");
    let used = state.tick();
    let chat = touch_chat(&mut state, chat_id);
    let previous = chat.paths.get(path);
    // A partial re-read of a version already seen in full keeps it full.
    let keep_full = previous
        .map(|p| p.full && p.sha256 == sha256)
        .unwrap_or(false);
    chat.paths.insert(
        path.to_owned(),
        PathEntry {
            sha256: sha256.to_owned(),
            full: full || keep_full,
            used,
        },
    );
    while chat.paths.len() > MAX_PATHS_PER_CHAT {
        let oldest_used = chat
            .paths
            .values()
            .map(|p| p.used)
            .min()
            .unwrap_or(u64::MAX);
        let victim = chat
            .paths
            .iter()
            .find(|(_, p)| p.used == oldest_used)
            .map(|(k, _)| k.clone());
        match victim {
            Some(key) => {
                chat.paths.remove(&key);
            }
            None => break,
        }
    }
}

pub fn record_read(chat_id: Option<&str>, canonical_path: &str, sha256: &str, full: bool) {
    put(chat_id, canonical_path, sha256, full);
}

pub fn record_write(chat_id: Option<&str>, canonical_path: &str, sha256: &str) {
    put(chat_id, canonical_path, sha256, true);
}

pub fn expected(chat_id: Option<&str>, canonical_path: &str) -> Option<LedgerEntry> {
    let chat_id = chat_id?;
    let mut state = LEDGER.lock().expect("version ledger lock poisoned");
    let used = state.tick();
    let chat = state.chats.get_mut(chat_id)?;
    let entry = chat.paths.get_mut(canonical_path)?;
    // Re-reading an entry marks it most recently used (without touching the
    // chat's own recency, matching the TS Map.get path).
    entry.used = used;
    Some(LedgerEntry {
        sha256: entry.sha256.clone(),
        full: entry.full,
    })
}

/// Test seam.
pub fn clear_version_ledger() {
    let mut state = LEDGER.lock().expect("version ledger lock poisoned");
    state.chats.clear();
    state.clock = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All ledger tests share the one process-global ledger, so they run
    /// strictly one at a time.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static TEST_LOCK: Mutex<()> = Mutex::new(());
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn read_then_expected_reports_the_recorded_version() {
        let _g = lock();
        clear_version_ledger();
        record_read(Some("c1"), "/p/a.txt", "aa", true);
        let e = expected(Some("c1"), "/p/a.txt").expect("recorded");
        assert_eq!(e.sha256, "aa");
        assert!(e.full);
        assert!(expected(Some("c1"), "/p/missing.txt").is_none());
        assert!(expected(Some("other-chat"), "/p/a.txt").is_none());
    }

    #[test]
    fn no_chat_id_records_nothing() {
        let _g = lock();
        clear_version_ledger();
        record_read(None, "/p/x.txt", "bb", true);
        record_write(None, "/p/y.txt", "cc");
        assert!(expected(None, "/p/x.txt").is_none());
        assert!(expected(None, "/p/y.txt").is_none());
    }

    #[test]
    fn partial_read_records_not_full_and_write_promotes() {
        let _g = lock();
        clear_version_ledger();
        record_read(Some("c2"), "/p/f", "v1", false);
        assert!(!expected(Some("c2"), "/p/f").unwrap().full);
        record_write(Some("c2"), "/p/f", "v1");
        assert!(expected(Some("c2"), "/p/f").unwrap().full);
    }

    #[test]
    fn partial_reread_of_a_full_version_keeps_it_full() {
        let _g = lock();
        clear_version_ledger();
        record_read(Some("c3"), "/p/g", "same", true);
        record_read(Some("c3"), "/p/g", "same", false);
        assert!(expected(Some("c3"), "/p/g").unwrap().full);
        // A partial read of a DIFFERENT version demotes to full:false.
        record_read(Some("c3"), "/p/g", "new", false);
        assert!(!expected(Some("c3"), "/p/g").unwrap().full);
    }

    #[test]
    fn stale_version_is_visible_as_stale() {
        let _g = lock();
        clear_version_ledger();
        record_read(Some("c4"), "/p/h", "old", true);
        // After an external change, the ledger still says "old": the caller
        // compares against the on-disk hash and raises file_changed.
        let e = expected(Some("c4"), "/p/h").unwrap();
        assert_eq!(e.sha256, "old");
        record_write(Some("c4"), "/p/h", "new");
        assert_eq!(expected(Some("c4"), "/p/h").unwrap().sha256, "new");
    }

    #[test]
    fn path_lru_evicts_oldest_beyond_the_bound() {
        let _g = lock();
        clear_version_ledger();
        for i in 0..MAX_PATHS_PER_CHAT + 2 {
            record_read(Some("c5"), &format!("/p/{i}"), "aa", true);
        }
        assert!(expected(Some("c5"), "/p/0").is_none());
        assert!(expected(Some("c5"), "/p/1").is_none());
        assert!(expected(Some("c5"), &format!("/p/{MAX_PATHS_PER_CHAT}")).is_some());
        clear_version_ledger();
    }

    #[test]
    fn chat_lru_evicts_oldest_beyond_the_bound() {
        let _g = lock();
        clear_version_ledger();
        for i in 0..MAX_CHATS + 2 {
            record_read(Some(&format!("chat-{i}")), "/p/a", "aa", true);
        }
        assert!(expected(Some("chat-0"), "/p/a").is_none());
        assert!(expected(Some("chat-1"), "/p/a").is_none());
        assert!(expected(Some(&format!("chat-{MAX_CHATS}")), "/p/a").is_some());
        clear_version_ledger();
    }
}
