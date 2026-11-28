//! Egress bridges (ADR 0040 §3): every guest connection to the egress port
//! is spliced to a new connection to the host's egress socket, on plain
//! descriptors, so this is tested without a VM.
//!
//! - At most [`EgressSlots`] bridges run at once; a connection beyond that
//!   is refused where the framework offers it and never reaches a bridge.
//! - The guest's end of stream is passed on as a half close (a request is
//!   complete, its answer may follow). The proxy's end of stream ends the
//!   bridge and shuts the guest side down; a failed side ends both.
//! - A bridge no byte crossed for the idle limit is shut down by the
//!   [`IdleWatch`] sweep; `shutdown` wakes both blocked directions.
//! - A bridge holds its two descriptors and nothing more: both directions
//!   share them.
//! - A guest can make egress fail at will, so egress diagnostics go
//!   through a [`DiagLimit`].

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

/// Bytes moved per read.
const CHUNK: usize = 16 * 1024;

/// The bound on bridges running at once.
#[derive(Clone)]
pub(crate) struct EgressSlots {
    active: Arc<AtomicUsize>,
    max: usize,
}

/// One taken slot; dropping it frees the slot, whatever path it took.
pub(crate) struct Slot(Arc<AtomicUsize>);

impl EgressSlots {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    /// A slot, or `None` while `max` are taken.
    pub(crate) fn try_take(&self) -> Option<Slot> {
        self.active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < self.max).then_some(n + 1)
            })
            .ok()
            .map(|_| Slot(Arc::clone(&self.active)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One bridged connection, shared by its two directions and the sweep.
pub(crate) struct Bridge {
    guest: UnixStream,
    host: UnixStream,
    born: Instant,
    /// Milliseconds after `born` when a byte last crossed.
    last_ms: AtomicU64,
}

impl Bridge {
    pub(crate) fn new(guest: UnixStream, host: UnixStream) -> Arc<Self> {
        Arc::new(Self {
            guest,
            host,
            born: Instant::now(),
            last_ms: AtomicU64::new(0),
        })
    }

    /// Run both directions until the bridge ended.
    pub(crate) fn run(self: &Arc<Self>) {
        let upload = {
            let bridge = Arc::clone(self);
            thread::spawn(move || match bridge.pump(&bridge.guest, &bridge.host) {
                Ok(()) => {
                    let _ = bridge.host.shutdown(Shutdown::Write);
                }
                Err(_) => bridge.shut_both(),
            })
        };
        match self.pump(&self.host, &self.guest) {
            // The proxy is done; ending the guest side also ends the upload.
            Ok(()) => {
                let _ = self.guest.shutdown(Shutdown::Both);
            }
            Err(_) => self.shut_both(),
        }
        let _ = upload.join();
    }

    /// Copy `from` to `to` until `from` ends; `Err` when either failed.
    fn pump(&self, from: &UnixStream, to: &UnixStream) -> io::Result<()> {
        let (mut from, mut to) = (from, to);
        let mut buf = [0_u8; CHUNK];
        loop {
            let read = match from.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            to.write_all(&buf[..read])?;
            self.touch();
        }
    }

    fn touch(&self) {
        let now = u64::try_from(self.born.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(now, Ordering::SeqCst);
    }

    fn idle_at(&self, now: Instant) -> Duration {
        let last = self.born + Duration::from_millis(self.last_ms.load(Ordering::SeqCst));
        now.saturating_duration_since(last)
    }

    fn shut_both(&self) {
        let _ = self.guest.shutdown(Shutdown::Both);
        let _ = self.host.shutdown(Shutdown::Both);
    }
}

/// The bridges an idle limit applies to.
pub(crate) struct IdleWatch {
    limit: Duration,
    bridges: Mutex<Vec<Weak<Bridge>>>,
}

impl IdleWatch {
    pub(crate) fn new(limit: Duration) -> Self {
        Self {
            limit,
            bridges: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn watch(&self, bridge: &Arc<Bridge>) {
        self.lock().push(Arc::downgrade(bridge));
    }

    /// Shut down every bridge idle for the limit at `now` and forget the
    /// ended ones; answers how many were shut down.
    pub(crate) fn sweep(&self, now: Instant) -> usize {
        let mut shut = 0;
        self.lock().retain(|weak| {
            let Some(bridge) = weak.upgrade() else {
                return false;
            };
            if bridge.idle_at(now) >= self.limit {
                bridge.shut_both();
                shut += 1;
                return false;
            }
            true
        });
        shut
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Weak<Bridge>>> {
        self.bridges.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Diagnostic lines of one kind, at most one per `every`: the ones in
/// between are counted, and the next line written says how many.
pub(crate) struct DiagLimit {
    every: Duration,
    state: Mutex<(Option<Instant>, u64)>,
}

impl DiagLimit {
    pub(crate) fn new(every: Duration) -> Self {
        Self {
            every,
            state: Mutex::new((None, 0)),
        }
    }

    /// The line to write for `text` at `now`, or `None` when it is held
    /// back (and counted).
    pub(crate) fn line(&self, now: Instant, text: &str) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (last, held) = &mut *state;
        if last.is_some_and(|last| now.saturating_duration_since(last) < self.every) {
            *held += 1;
            return None;
        }
        *last = Some(now);
        let line = match std::mem::take(held) {
            0 => text.to_owned(),
            more => format!("{text} (and {more} more since the last such line)"),
        };
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridged() -> (UnixStream, UnixStream, Arc<Bridge>) {
        let (guest, guest_end) = UnixStream::pair().unwrap();
        let (proxy_end, proxy) = UnixStream::pair().unwrap();
        (guest, proxy, Bridge::new(guest_end, proxy_end))
    }

    #[test]
    fn the_guest_end_of_stream_is_a_half_close() {
        let (mut guest, mut proxy, bridge) = bridged();
        let running = thread::spawn(move || bridge.run());
        guest.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        guest.shutdown(Shutdown::Write).unwrap();
        let mut request = Vec::new();
        proxy.read_to_end(&mut request).unwrap();
        assert_eq!(request, b"GET / HTTP/1.0\r\n\r\n");
        proxy.write_all(b"HTTP/1.0 200 OK\r\n\r\n").unwrap();
        drop(proxy);
        let mut response = Vec::new();
        guest.read_to_end(&mut response).unwrap();
        assert_eq!(response, b"HTTP/1.0 200 OK\r\n\r\n");
        running.join().unwrap();
    }

    #[test]
    fn the_proxy_end_of_stream_ends_the_bridge() {
        let (mut guest, mut proxy, bridge) = bridged();
        let running = thread::spawn(move || bridge.run());
        proxy.write_all(b"bye").unwrap();
        drop(proxy);
        // The guest never half-closes; the bridge ends all the same.
        let mut rest = Vec::new();
        guest.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"bye");
        running.join().unwrap();
    }

    #[test]
    fn a_sweep_shuts_only_idle_bridges() {
        let watch = IdleWatch::new(Duration::from_millis(300));
        let (mut idle_guest, _idle_proxy, idle) = bridged();
        let (mut busy_guest, mut busy_proxy, busy) = bridged();
        watch.watch(&idle);
        watch.watch(&busy);
        let idle_run = thread::spawn(move || idle.run());
        let busy_run = thread::spawn(move || busy.run());
        let mut shut = 0;
        for _ in 0..15 {
            busy_guest.write_all(b"x").unwrap();
            let mut byte = [0_u8; 1];
            busy_proxy.read_exact(&mut byte).unwrap();
            shut += watch.sweep(Instant::now());
            thread::sleep(Duration::from_millis(30));
        }
        assert_eq!(shut, 1, "the idle bridge only");
        idle_run.join().unwrap();
        let mut rest = Vec::new();
        idle_guest.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        drop(busy_proxy);
        busy_run.join().unwrap();
        assert_eq!(
            watch.sweep(Instant::now()),
            0,
            "ended bridges are forgotten"
        );
    }

    #[test]
    fn slots_are_bounded_and_freed_on_drop() {
        let slots = EgressSlots::new(2);
        let (a, b) = (slots.try_take().unwrap(), slots.try_take().unwrap());
        assert!(slots.try_take().is_none());
        drop(a);
        let c = slots.try_take().unwrap();
        assert!(slots.try_take().is_none());
        drop((b, c));
        assert_eq!(slots.active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn diagnostics_are_limited_and_counted() {
        let limit = DiagLimit::new(Duration::from_secs(10));
        let start = Instant::now();
        assert_eq!(limit.line(start, "failed").as_deref(), Some("failed"));
        for ms in [1, 2, 9_999] {
            assert!(
                limit
                    .line(start + Duration::from_millis(ms), "failed")
                    .is_none()
            );
        }
        let later = start + Duration::from_secs(10);
        assert_eq!(
            limit.line(later, "failed").as_deref(),
            Some("failed (and 3 more since the last such line)")
        );
        assert!(limit.line(later, "failed").is_none());
    }
}
