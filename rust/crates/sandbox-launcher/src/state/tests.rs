use super::*;

fn ws(id: &str) -> WorkspaceId {
    WorkspaceId::parse(id).unwrap()
}

#[tokio::test]
async fn records_survive_a_reopen_and_only_grow() {
    let dir = tempfile::tempdir().unwrap();
    let store = StateStore::open(dir.path()).unwrap();
    assert_eq!(store.get(&ws("a")).await, Fence::default());
    store.record_stopped(&ws("a"), 3).await.unwrap();
    store.record_stopped(&ws("a"), 2).await.unwrap();
    store.record_created(&ws("a"), 4).await.unwrap();
    store.record_created(&ws("b"), 1).await.unwrap();
    drop(store);
    let reopened = StateStore::open(dir.path()).unwrap();
    let a = reopened.get(&ws("a")).await;
    assert_eq!((a.stopped_through, a.created_through), (3, 4));
    assert_eq!(reopened.get(&ws("b")).await.created_through, 1);
    assert_eq!(reopened.len().await, 2);
    assert!(!dir.path().join(TEMP_NAME).exists());
}

#[tokio::test]
async fn a_leftover_temp_file_or_link_is_replaced_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("elsewhere");
    std::fs::write(&target, b"keep").unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join(TEMP_NAME)).unwrap();
    let store = StateStore::open(dir.path()).unwrap();
    store.record_stopped(&ws("a"), 1).await.unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    drop(store);
    assert_eq!(
        StateStore::open(dir.path())
            .unwrap()
            .get(&ws("a"))
            .await
            .stopped_through,
        1
    );
}

#[test]
fn an_unreadable_state_file_stops_the_launcher() {
    let cases: [&[u8]; 5] = [
        b"not json",
        br#"{"version":2,"workspaces":{}}"#,
        br#"{"version":1,"workspaces":{"../x":{"stopped_through":1}}}"#,
        br#"{"version":1,"workspaces":{"a":{"stopped_through":-1}}}"#,
        br#"{"version":1,"workspaces":{"a":{"stopped":1}}}"#,
    ];
    for bytes in cases {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), bytes).unwrap();
        let error = StateStore::open(dir.path()).unwrap_err();
        assert!(matches!(error, StateError::Invalid(_)), "{error}");
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(FILE_NAME)).unwrap();
    assert!(
        StateStore::open(dir.path()).is_err(),
        "a directory is no state file"
    );
}

#[tokio::test]
async fn one_launcher_per_state_directory() {
    let dir = tempfile::tempdir().unwrap();
    let first = StateStore::open(dir.path()).unwrap();
    let second = StateStore::open(dir.path()).unwrap_err();
    assert!(matches!(second, StateError::Locked), "{second}");
    first.record_created(&ws("a"), 1).await.unwrap();
    drop(first);
    let reopened = StateStore::open(dir.path()).unwrap();
    assert_eq!(reopened.get(&ws("a")).await.created_through, 1);
}

#[test]
fn a_missing_state_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    assert!(StateStore::open(&dir.path().join("absent")).is_err());
}
