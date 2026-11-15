//! Conformance suite for `agentkit_core::OutputStore` (roadmap 2.1, ADR 0034):
//! the behaviour the SQLite adapter and the in-memory twin must share. Each
//! case runs on a fixture built fresh by `run`. Facts the port cannot read
//! (artifacts still held, subagent links, time) come from
//! [`OutputStoreFixture`].

use agentkit_core::{
    ByteOrLineRange, OUTPUT_RETENTION_MS, OutputError, OutputHandle, OutputMeta, OutputStore,
    PurgeReport, SessionTree,
};
use async_trait::async_trait;
use std::{future::Future, pin::Pin};

pub const START_MS: i64 = 1_760_000_000_000;

#[async_trait]
pub trait OutputStoreFixture: Clone + Send + Sync + 'static {
    type Store: OutputStore + SessionTree;

    fn store(&self) -> &Self::Store;

    /// Move the fixture's clock to `now_ms`.
    fn set_now(&self, now_ms: i64);

    /// Artifacts still held for a session: files on disk for the SQLite
    /// adapter, entries for the twin.
    async fn artifacts(&self, session: &str) -> usize;

    /// Register `child` as a subagent session of `parent`.
    async fn link_child(&self, child: &str, parent: &str);

    /// Run the retention sweep.
    async fn purge_expired(&self) -> PurgeReport;
}

type CaseFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

macro_rules! suite {
    ($($name:ident),+ $(,)?) => {
        /// Names of every case, in run order.
        pub const CASES: &[&str] = &[$(stringify!($name)),+];
        fn cases<T: OutputStoreFixture>() -> Vec<(&'static str, fn(T) -> CaseFuture)> {
            vec![$((stringify!($name), (|fixture| Box::pin($name(fixture))) as fn(T) -> CaseFuture)),+]
        }
    };
}

suite!(
    put_returns_handle_and_index_facts,
    handles_are_unique,
    empty_output_is_stored,
    read_lines_returns_exact_ranges,
    read_lines_past_the_end_is_empty,
    read_lines_ignores_one_trailing_newline,
    read_bytes_never_cuts_a_character,
    large_output_roundtrips_exactly,
    search_finds_lines_by_pattern_and_limit,
    search_falls_back_to_literal_text,
    search_refuses_an_empty_pattern,
    unknown_handle_is_not_found,
    purge_session_removes_rows_and_files,
    retention_ends_after_thirty_days_without_activity,
    activity_moves_retention_forward,
    purge_expired_removes_rows_and_files,
    may_read_follows_descendants_only,
);

/// Runs every case against a fixture built fresh by `make`. Panics, naming
/// the case, on the first failure.
pub async fn run<T, F, Fut>(make: F)
where
    T: OutputStoreFixture,
    F: Fn() -> Fut,
    Fut: Future<Output = T>,
{
    for (name, case) in cases::<T>() {
        let fixture = make().await;
        fixture.set_now(START_MS);
        if let Err(error) = tokio::spawn(case(fixture)).await {
            let text = if error.is_panic() {
                let payload = error.into_panic();
                match payload.downcast::<String>() {
                    Ok(text) => *text,
                    Err(payload) => payload
                        .downcast::<&str>()
                        .map_or_else(|_| "non-text panic".to_owned(), |text| (*text).to_owned()),
                }
            } else {
                format!("case task cancelled: {error}")
            };
            panic!("output store conformance case `{name}` failed: {text}");
        }
    }
}

fn meta(session: &str) -> OutputMeta {
    OutputMeta {
        session: session.into(),
        task: "task-1".into(),
        call_id: "call-1".into(),
        tool: "run_command".into(),
        mime: "text/plain".into(),
        exit_code: Some(0),
    }
}

fn numbered(lines: usize) -> String {
    (1..=lines)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn store_text<T: OutputStoreFixture>(
    fixture: &T,
    session: &str,
    text: &str,
) -> OutputHandle {
    fixture
        .store()
        .put(meta(session), text.as_bytes().to_vec())
        .await
        .expect("put")
}

fn lines(start_line: u64, max_lines: u64) -> ByteOrLineRange {
    ByteOrLineRange::Lines {
        start_line,
        max_lines,
    }
}

// ---- cases -----------------------------------------------------------------

async fn put_returns_handle_and_index_facts<T: OutputStoreFixture>(fixture: T) {
    let text = "alpha\nbeta\ngamma\n";
    let handle = store_text(&fixture, "s1", text).await;
    assert!(handle.as_str().starts_with("out_"));
    assert_eq!(handle.as_str().len(), 4 + 32);
    assert_eq!(OutputHandle::parse(handle.as_str()).expect("parse"), handle);
    let info = fixture.store().info(&handle).await.expect("info");
    assert_eq!(info.handle, handle);
    assert_eq!(info.meta, meta("s1"));
    assert_eq!(info.byte_len, text.len() as u64);
    assert_eq!(info.line_count, 3);
    assert_eq!(info.sha256, agentkit_kernel::sha256_hex(text));
    assert_eq!(info.created_ms, START_MS);
    assert_eq!(info.expires_ms, START_MS + OUTPUT_RETENTION_MS);
}

async fn handles_are_unique<T: OutputStoreFixture>(fixture: T) {
    let first = store_text(&fixture, "s1", "same").await;
    let second = store_text(&fixture, "s1", "same").await;
    assert_ne!(first, second);
}

async fn empty_output_is_stored<T: OutputStoreFixture>(fixture: T) {
    let handle = store_text(&fixture, "s1", "").await;
    let info = fixture.store().info(&handle).await.expect("info");
    assert_eq!((info.byte_len, info.line_count), (0, 0));
    let slice = fixture.store().read(&handle, lines(1, 10)).await.expect("read");
    assert_eq!(slice.text, "");
    assert!(!slice.has_more);
}

async fn read_lines_returns_exact_ranges<T: OutputStoreFixture>(fixture: T) {
    let handle = store_text(&fixture, "s1", &numbered(100)).await;
    let slice = fixture.store().read(&handle, lines(10, 5)).await.expect("read");
    assert_eq!(slice.text, "line 10\nline 11\nline 12\nline 13\nline 14");
    assert_eq!((slice.start_line, slice.end_line), (10, 14));
    assert_eq!(slice.total_lines, 100);
    assert!(slice.has_more);
    let tail = fixture.store().read(&handle, lines(98, 50)).await.expect("read");
    assert_eq!(tail.text, "line 98\nline 99\nline 100");
    assert_eq!((tail.start_line, tail.end_line), (98, 100));
    assert!(!tail.has_more);
    let first = fixture.store().read(&handle, lines(0, 1)).await.expect("read");
    assert_eq!(first.text, "line 1");
    assert_eq!(first.start_line, 1);
}

async fn read_lines_past_the_end_is_empty<T: OutputStoreFixture>(fixture: T) {
    let handle = store_text(&fixture, "s1", &numbered(3)).await;
    let slice = fixture.store().read(&handle, lines(4, 5)).await.expect("read");
    assert_eq!(slice.text, "");
    assert_eq!(slice.total_lines, 3);
    assert!(!slice.has_more);
}

async fn read_lines_ignores_one_trailing_newline<T: OutputStoreFixture>(fixture: T) {
    let handle = store_text(&fixture, "s1", "a\nb\n").await;
    let slice = fixture.store().read(&handle, lines(1, 10)).await.expect("read");
    assert_eq!(slice.text, "a\nb");
    assert_eq!(slice.total_lines, 2);
}

async fn read_bytes_never_cuts_a_character<T: OutputStoreFixture>(fixture: T) {
    // h, é (two bytes), l, l, o
    let handle = store_text(&fixture, "s1", "héllo").await;
    let slice = fixture
        .store()
        .read(&handle, ByteOrLineRange::Bytes { start: 2, len: 3 })
        .await
        .expect("read");
    assert!(!slice.text.contains('\u{fffd}'), "{:?}", slice.text);
    assert_eq!(slice.text, "ll");
    let whole = fixture
        .store()
        .read(&handle, ByteOrLineRange::Bytes { start: 0, len: 100 })
        .await
        .expect("read");
    assert_eq!(whole.text, "héllo");
    assert!(!whole.has_more);
}

async fn large_output_roundtrips_exactly<T: OutputStoreFixture>(fixture: T) {
    // A little over 1 MiB: 1 MiB is the CE-10 size.
    let text = numbered(110_000);
    assert!(text.len() > 1024 * 1024);
    let handle = store_text(&fixture, "s1", &text).await;
    let info = fixture.store().info(&handle).await.expect("info");
    assert_eq!(info.byte_len, text.len() as u64);
    assert_eq!(info.line_count, 110_000);
    let all = fixture
        .store()
        .read(&handle, lines(1, 1_000_000))
        .await
        .expect("read");
    assert_eq!(all.text, text);
    assert!(!all.has_more);
    let middle = fixture
        .store()
        .read(&handle, lines(54_321, 3))
        .await
        .expect("read");
    assert_eq!(middle.text, "line 54321\nline 54322\nline 54323");
}

async fn search_finds_lines_by_pattern_and_limit<T: OutputStoreFixture>(fixture: T) {
    let handle = store_text(&fixture, "s1", &numbered(100)).await;
    let found = fixture
        .store()
        .search(&handle, r"^line 1[0-2]$", 10)
        .await
        .expect("search");
    let seen: Vec<(u64, &str)> = found.iter().map(|m| (m.line, m.text.as_str())).collect();
    assert_eq!(seen, vec![(10, "line 10"), (11, "line 11"), (12, "line 12")]);
    let limited = fixture.store().search(&handle, "line", 4).await.expect("search");
    assert_eq!(limited.len(), 4);
    assert_eq!(limited[3].line, 4);
    let none = fixture.store().search(&handle, "absent", 4).await.expect("search");
    assert!(none.is_empty());
}

async fn search_falls_back_to_literal_text<T: OutputStoreFixture>(fixture: T) {
    let handle = store_text(&fixture, "s1", "plain\nf(x) = 1\nother").await;
    // `f(x` is not a regular expression; it is searched as text.
    let found = fixture.store().search(&handle, "f(x", 5).await.expect("search");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line, 2);
}

async fn search_refuses_an_empty_pattern<T: OutputStoreFixture>(fixture: T) {
    let handle = store_text(&fixture, "s1", "a").await;
    let error = fixture.store().search(&handle, "", 5).await.expect_err("refused");
    assert!(matches!(error, OutputError::Invalid(_)), "{error:?}");
}

async fn unknown_handle_is_not_found<T: OutputStoreFixture>(fixture: T) {
    let unknown = OutputHandle::generate();
    for outcome in [
        fixture.store().info(&unknown).await.map(|_| ()),
        fixture.store().read(&unknown, lines(1, 1)).await.map(|_| ()),
        fixture.store().search(&unknown, "x", 1).await.map(|_| ()),
    ] {
        assert!(matches!(outcome, Err(OutputError::NotFound(_))), "{outcome:?}");
    }
}

async fn purge_session_removes_rows_and_files<T: OutputStoreFixture>(fixture: T) {
    let first = store_text(&fixture, "s1", "one").await;
    let second = store_text(&fixture, "s1", "three").await;
    let other = store_text(&fixture, "s2", "keep").await;
    assert_eq!(fixture.artifacts("s1").await, 2);
    let report = fixture.store().purge_session("s1").await.expect("purge");
    assert_eq!(report, PurgeReport { outputs: 2, bytes: 8 });
    assert_eq!(fixture.artifacts("s1").await, 0);
    for handle in [&first, &second] {
        assert!(matches!(
            fixture.store().info(handle).await,
            Err(OutputError::NotFound(_))
        ));
    }
    assert_eq!(fixture.artifacts("s2").await, 1);
    fixture.store().info(&other).await.expect("other session kept");
    let again = fixture.store().purge_session("s1").await.expect("purge again");
    assert_eq!(again, PurgeReport::default());
}

async fn retention_ends_after_thirty_days_without_activity<T: OutputStoreFixture>(fixture: T) {
    let handle = store_text(&fixture, "s1", "kept a while").await;
    fixture.set_now(START_MS + OUTPUT_RETENTION_MS - 1);
    fixture.store().info(&handle).await.expect("still live");
    fixture.set_now(START_MS + OUTPUT_RETENTION_MS);
    assert!(matches!(
        fixture.store().info(&handle).await,
        Err(OutputError::NotFound(_))
    ));
}

async fn activity_moves_retention_forward<T: OutputStoreFixture>(fixture: T) {
    let day = 24 * 60 * 60 * 1000;
    let first = store_text(&fixture, "s1", "first").await;
    fixture.set_now(START_MS + 20 * day);
    // A read on the session moves every output of that session forward.
    fixture
        .store()
        .read(&first, lines(1, 1))
        .await
        .expect("read");
    fixture.set_now(START_MS + 40 * day);
    fixture.store().info(&first).await.expect("still live at day 40");
    fixture.set_now(START_MS + 51 * day);
    assert!(matches!(
        fixture.store().info(&first).await,
        Err(OutputError::NotFound(_))
    ));
}

async fn purge_expired_removes_rows_and_files<T: OutputStoreFixture>(fixture: T) {
    store_text(&fixture, "old", "stale bytes").await;
    fixture.set_now(START_MS + OUTPUT_RETENTION_MS / 2);
    store_text(&fixture, "young", "fresh").await;
    fixture.set_now(START_MS + OUTPUT_RETENTION_MS + 1);
    let report = fixture.purge_expired().await;
    assert_eq!(report, PurgeReport { outputs: 1, bytes: 11 });
    assert_eq!(fixture.artifacts("old").await, 0);
    assert_eq!(fixture.artifacts("young").await, 1);
}

async fn may_read_follows_descendants_only<T: OutputStoreFixture>(fixture: T) {
    fixture.link_child("child-a", "root").await;
    fixture.link_child("child-b", "root").await;
    fixture.link_child("grandchild", "child-a").await;
    let store = fixture.store();
    assert!(store.may_read("root", "root").await);
    assert!(store.may_read("root", "child-a").await);
    assert!(store.may_read("root", "grandchild").await);
    assert!(store.may_read("child-a", "grandchild").await);
    // Never up, and never across.
    assert!(!store.may_read("child-a", "root").await);
    assert!(!store.may_read("grandchild", "root").await);
    assert!(!store.may_read("child-a", "child-b").await);
    assert!(!store.may_read("child-b", "grandchild").await);
    assert!(!store.may_read("root", "elsewhere").await);
    assert!(!store.may_read("child-a", "elsewhere").await);
}
