//! The control bridge between the guest's vsock connection and the host's
//! control socket, on plain descriptors (a vsock descriptor supports every
//! call a `UnixStream` makes), so it is tested without a VM. It ends as
//! soon as either side closes or fails, and then shuts both down: the
//! guest's session is over (ADR 0039, "no host-loss survival"). Both
//! directions share each stream through an `Arc`, so no descriptor is
//! duplicated. Egress bridges are [`crate::egress`].

use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// Pause between connect attempts.
const RETRY: Duration = Duration::from_millis(50);

/// Connect to the unix socket at `path`, retrying while it is missing or
/// refuses (the adapter binds it only after `ensured`) until `deadline` or
/// until `cancelled` answers true.
pub(crate) fn connect_until(
    path: &Path,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> io::Result<UnixStream> {
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error) if retryable(&error) && Instant::now() < deadline && !cancelled() => {
                thread::sleep(RETRY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Copy both ways until either side ends, then shut both down.
pub(crate) fn splice_until_either_closes(a: Arc<UnixStream>, b: Arc<UnixStream>) {
    let (a2, b2) = (Arc::clone(&a), Arc::clone(&b));
    let forward = thread::spawn(move || {
        let _ = io::copy(&mut &*a2, &mut &*b2);
        shut_both(&a2, &b2);
    });
    let _ = io::copy(&mut &*b, &mut &*a);
    shut_both(&a, &b);
    let _ = forward.join();
}

fn shut_both(a: &UnixStream, b: &UnixStream) {
    let _ = a.shutdown(Shutdown::Both);
    let _ = b.shutdown(Shutdown::Both);
}

/// Streams to shut down when the helper stops. Once closed, a stream added
/// later is shut down at once.
#[derive(Clone, Default)]
pub(crate) struct Closer(Arc<Mutex<CloserState>>);

#[derive(Default)]
struct CloserState {
    closed: bool,
    streams: Vec<Arc<UnixStream>>,
}

impl Closer {
    /// Shut `stream` down at [`Closer::close`]; false (and shut down now)
    /// when that already happened.
    pub(crate) fn add(&self, stream: &Arc<UnixStream>) -> bool {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            let _ = stream.shutdown(Shutdown::Both);
            return false;
        }
        state.streams.push(Arc::clone(stream));
        true
    }

    /// Shut every added stream down.
    pub(crate) fn close(&self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        for stream in state.streams.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).closed
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn connect_waits_for_a_listener_bound_late_and_gives_up_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let late = path.clone();
        let binder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            UnixListener::bind(&late).unwrap()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = connect_until(&path, deadline, || false).unwrap();
        let listener = binder.join().unwrap();
        drop((stream, listener));
        let missing = dir.path().join("missing.sock");
        let soon = Instant::now() + Duration::from_millis(100);
        assert!(connect_until(&missing, soon, || false).is_err());
        let far = Instant::now() + Duration::from_secs(60);
        assert!(connect_until(&missing, far, || true).is_err(), "cancelled");
    }

    #[test]
    fn the_control_bridge_ends_when_one_side_closes() {
        let (guest, guest_end) = UnixStream::pair().unwrap();
        let (host, mut host_end) = UnixStream::pair().unwrap();
        let (guest_end, host) = (Arc::new(guest_end), Arc::new(host));
        let bridge = thread::spawn(move || splice_until_either_closes(guest_end, host));
        let mut guest = guest;
        guest.write_all(b"hello").unwrap();
        let mut buf = [0_u8; 5];
        host_end.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        host_end.write_all(b"back").unwrap();
        let mut back = [0_u8; 4];
        guest.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"back");
        drop(host_end);
        bridge.join().unwrap();
        let mut rest = Vec::new();
        guest.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "the guest side was shut down");
    }

    #[test]
    fn a_closer_shuts_streams_down_now_and_later() {
        let closer = Closer::default();
        let (a, mut a_peer) = UnixStream::pair().unwrap();
        assert!(closer.add(&Arc::new(a)));
        closer.close();
        let mut buf = Vec::new();
        a_peer.read_to_end(&mut buf).unwrap();
        let (b, mut b_peer) = UnixStream::pair().unwrap();
        assert!(!closer.add(&Arc::new(b)) && closer.is_closed());
        b_peer.read_to_end(&mut buf).unwrap();
        assert!(buf.is_empty());
    }
}
