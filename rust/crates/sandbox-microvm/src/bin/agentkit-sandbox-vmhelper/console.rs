//! The guest's `hvc0` console (ADR 0040 §3). The VM writes it into a pipe
//! that a thread drains into the console log: the first
//! [`CONSOLE_LOG_MAX_BYTES`] are kept, then one line notes the cut and the
//! rest is read and dropped. The file stays bounded whatever the guest
//! prints, and the pipe is always drained, so the guest never blocks on
//! its console; a log that fails a write gets nothing more.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Bytes of console output the log keeps.
pub(crate) const CONSOLE_LOG_MAX_BYTES: u64 = 1024 * 1024;
/// Bytes read from the pipe per call.
const CHUNK: usize = 16 * 1024;
/// How often [`Drain::settle`] looks.
const SETTLE_POLL: Duration = Duration::from_millis(10);

/// A running drain.
pub(crate) struct Drain {
    born: Instant,
    /// Milliseconds after `born` of the last read that returned bytes.
    last_ms: AtomicU64,
}

impl Drain {
    /// Drain `from` into `to` on a thread of its own, keeping `max` bytes.
    pub(crate) fn spawn(
        from: impl Read + Send + 'static,
        to: impl Write + Send + 'static,
        max: u64,
    ) -> Arc<Self> {
        let drain = Arc::new(Self {
            born: Instant::now(),
            last_ms: AtomicU64::new(0),
        });
        let running = Arc::clone(&drain);
        thread::spawn(move || copy_capped(from, to, max, &running));
        drain
    }

    /// Wait until no console output arrived for `quiet`, at most `within`:
    /// the last lines before a stop are the useful ones.
    pub(crate) fn settle(&self, quiet: Duration, within: Duration) {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            let last = self.born + Duration::from_millis(self.last_ms.load(Ordering::SeqCst));
            if last.elapsed() >= quiet {
                return;
            }
            thread::sleep(SETTLE_POLL);
        }
    }

    fn touch(&self) {
        let now = u64::try_from(self.born.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(now, Ordering::SeqCst);
    }
}

/// Copy `from` into `to` until `from` ends, keeping the first `max` bytes
/// and one note at the cut; answers the bytes kept.
fn copy_capped(mut from: impl Read, mut to: impl Write, max: u64, drain: &Drain) -> u64 {
    let mut buf = [0_u8; CHUNK];
    let (mut kept, mut writable, mut noted) = (0_u64, true, false);
    loop {
        let read = match from.read(&mut buf) {
            Ok(0) => return kept,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return kept,
        };
        drain.touch();
        let room = usize::try_from(max.saturating_sub(kept)).unwrap_or(usize::MAX);
        let keep = read.min(room);
        if writable && keep > 0 {
            writable = to.write_all(&buf[..keep]).is_ok();
            kept += keep as u64;
        }
        if writable && keep < read && !noted {
            let note = format!("\n[console log cut at {max} bytes; the rest was dropped]\n");
            writable = to.write_all(note.as_bytes()).is_ok();
            noted = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain() -> Drain {
        Drain {
            born: Instant::now(),
            last_ms: AtomicU64::new(0),
        }
    }

    /// Fails every write; counts the attempts.
    struct Broken(usize);

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.0 += 1;
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_log_keeps_the_first_bytes_and_one_note() {
        let input: Vec<u8> = (0..5000_u32).map(|i| (i % 251) as u8).collect();
        let mut from = io::Cursor::new(input.clone());
        let mut log = Vec::new();
        let kept = copy_capped(&mut from, &mut log, 1000, &drain());
        assert_eq!(kept, 1000);
        assert_eq!(&log[..1000], &input[..1000]);
        let note = String::from_utf8(log[1000..].to_vec()).unwrap();
        assert_eq!(
            note,
            "\n[console log cut at 1000 bytes; the rest was dropped]\n"
        );
        assert_eq!(from.position(), 5000, "everything was drained");
        let mut short = Vec::new();
        assert_eq!(copy_capped(&b"boot"[..], &mut short, 1000, &drain()), 4);
        assert_eq!(short, b"boot");
    }

    #[test]
    fn a_failing_log_gets_nothing_more_but_the_pipe_is_drained() {
        let mut from = io::Cursor::new(vec![7_u8; 100_000]);
        let mut broken = Broken(0);
        copy_capped(&mut from, &mut broken, CONSOLE_LOG_MAX_BYTES, &drain());
        assert_eq!(broken.0, 1, "one failed write, then none");
        assert_eq!(from.position(), 100_000);
    }

    #[test]
    fn settling_waits_for_a_quiet_pipe_but_not_forever() {
        let (reader, mut writer) = io::pipe().unwrap();
        let drain = Drain::spawn(reader, io::sink(), 1000);
        let begun = Instant::now();
        drain.settle(Duration::from_millis(50), Duration::from_secs(5));
        assert!(begun.elapsed() < Duration::from_secs(1), "quiet already");
        let chatty = thread::spawn(move || {
            for _ in 0..40 {
                writer.write_all(b"line\n").unwrap();
                thread::sleep(Duration::from_millis(10));
            }
        });
        thread::sleep(Duration::from_millis(30));
        let begun = Instant::now();
        drain.settle(Duration::from_secs(10), Duration::from_millis(200));
        assert!(begun.elapsed() >= Duration::from_millis(200), "bounded");
        chatty.join().unwrap();
    }
}
