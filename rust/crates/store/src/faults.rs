//! Process-local durability fault injection for the store durability cells.
//!
//! The plan is read from `AGENTKIT_STORE_FAULT` once per store open. Production
//! never sets the variable: with it unset the whole module collapses to an
//! absent thread-local and no extra SQL, so the production write path is
//! byte-for-byte unchanged. The seam exists because an integration test cannot
//! reach the actor's connection (the store owns it), and filesystem tricks
//! against the live `-wal` handle do not fail writes through an already-open
//! descriptor on macOS.
//!
//! Grammar: comma-separated tokens.
//! - `refuse=A:B` — after A further durability commits succeed on this
//!   store's actor thread, the next B refuse truthfully with an I/O error
//!   before SQLite ever sees a COMMIT; the enclosing transaction is rolled
//!   back, so nothing partial can persist and the store stays usable (the
//!   worker is not poisoned). The leading allow-list lets a cell commit its
//!   pre-fault state normally before the fault window opens.
//! - `fsync-off` — the durability synchronization is downgraded to `OFF`
//!   (simulated fsync no-op) for this store's connection.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{Result, StoreError};

#[derive(Default)]
pub(crate) struct StoreFaults {
    allow_commits: AtomicUsize,
    refuse_commits: AtomicUsize,
    pub(crate) fsync_off: bool,
}

fn spend(counter: &AtomicUsize) -> bool {
    // A saturating decrement: Ok means a credit existed and was spent.
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count > 0).then(|| count - 1)
        })
        .is_ok()
}

impl StoreFaults {
    /// Reads the fault plan from the environment; all-zero when unset.
    pub(crate) fn from_env() -> Self {
        match std::env::var("AGENTKIT_STORE_FAULT") {
            Ok(spec) => Self::from_spec(&spec),
            Err(_) => Self::default(),
        }
    }

    fn from_spec(spec: &str) -> Self {
        let mut faults = Self::default();
        for token in spec
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            if token == "fsync-off" {
                faults.fsync_off = true;
            } else if let Some(window) = token.strip_prefix("refuse=") {
                let (allow, refuse) = window.split_once(':').unwrap_or(("0", window));
                faults.allow_commits = AtomicUsize::new(allow.parse().unwrap_or(0));
                faults.refuse_commits = AtomicUsize::new(refuse.parse().unwrap_or(0));
            } else {
                // Fail closed on an unreadable plan: a typo in a fault name
                // must not silently run the cell without its fault.
                unreachable!("unknown store fault token {token:?}; cells must use a defined name");
            }
        }
        faults
    }

    /// Consumes one refusal credit; true means this durability commit must
    /// answer a truthful write refusal.
    pub(crate) fn take_commit_refusal(&self) -> bool {
        if spend(&self.allow_commits) {
            return false;
        }
        spend(&self.refuse_commits)
    }
}

/// Truthful refusal for an injected write failure at the commit point,
/// shaped like the errno-backed `SQLITE_IOERR` a real storage failure
/// produces, so it travels the same error channel as a genuine one.
pub(crate) fn injected_commit_refusal() -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ffi::ErrorCode::SystemIoFailure,
            extended_code: rusqlite::ffi::SQLITE_IOERR_WRITE,
        },
        Some("injected write refusal at the durability commit point".into()),
    )
}

/// The same refusal in the store's own error domain.
pub(crate) fn injected_write_refusal() -> StoreError {
    StoreError::Io(std::io::Error::other(
        "injected write refusal at the durability commit point",
    ))
}

thread_local! {
    static BOUND: std::cell::RefCell<Option<std::sync::Arc<StoreFaults>>> =
        const { std::cell::RefCell::new(None) };
}

/// Binds the fault plan to the calling (actor) thread.
pub(crate) fn bind(faults: std::sync::Arc<StoreFaults>) {
    BOUND.with(|bound| *bound.borrow_mut() = Some(faults));
}

/// True when this thread's store owes a refusal for the durability commit
/// now in progress.
pub(crate) fn take_commit_refusal() -> bool {
    BOUND.with(|bound| {
        bound
            .borrow()
            .as_ref()
            .is_some_and(|faults| faults.take_commit_refusal())
    })
}

/// Applies the synchronization downgrade when the fsync no-op is armed.
pub(crate) fn apply_synchronous(
    connection: &rusqlite::Connection,
    faults: &StoreFaults,
) -> Result<()> {
    if faults.fsync_off {
        connection.pragma_update(None, "synchronous", "OFF")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synchronous_pragma(faults: &StoreFaults) -> Result<i64> {
        let connection = rusqlite::Connection::open_in_memory()?;
        // The production baseline the schema applies before any fault plan.
        connection.pragma_update(None, "synchronous", "FULL")?;
        apply_synchronous(&connection, faults)?;
        Ok(connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?)
    }

    #[test]
    fn production_plan_keeps_durability_synchronization_full() {
        // Falsifies the seam being inert-by-accident in production: with no
        // plan armed the pragma must stay FULL (2).
        assert_eq!(synchronous_pragma(&StoreFaults::default()).unwrap(), 2);
    }

    #[test]
    fn fsync_noop_actually_downgrades_the_durability_pragma() {
        // Falsifies a neutered `apply_synchronous`: if the no-op injection
        // ever stops firing, this reads FULL back and fails.
        let faults = StoreFaults::from_spec("fsync-off");
        assert!(faults.fsync_off);
        assert_eq!(synchronous_pragma(&faults).unwrap(), 0);
    }

    #[test]
    fn refusal_window_spends_the_allow_list_then_refuses_exactly_once() {
        let faults = StoreFaults::from_spec("refuse=2:1");
        let refusal = (
            faults.take_commit_refusal(),
            faults.take_commit_refusal(),
            faults.take_commit_refusal(),
            faults.take_commit_refusal(),
        );
        assert_eq!(refusal, (false, false, true, false));
    }

    #[test]
    fn refusal_is_shaped_like_a_real_storage_error() {
        let rusqlite_shaped = injected_commit_refusal();
        assert!(
            matches!(&rusqlite_shaped, rusqlite::Error::SqliteFailure(error, Some(message))
                if error.code == rusqlite::ffi::ErrorCode::SystemIoFailure
                    && message.contains("injected write refusal")),
            "{rusqlite_shaped:?}"
        );
        assert!(matches!(
            injected_write_refusal(),
            StoreError::Io(error) if error.to_string().contains("injected write refusal")
        ));
    }
}
