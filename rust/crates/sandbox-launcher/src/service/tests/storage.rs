//! `reset_home`, `destroy` and `usage`.

use super::*;

fn reset(ws: &str) -> Request {
    Request::ResetHome {
        workspace_id: ws.into(),
    }
}

fn destroy(ws: &str) -> Request {
    Request::Destroy {
        workspace_id: ws.into(),
    }
}

fn usage(ws: &str) -> Request {
    Request::Usage {
        workspace_id: ws.into(),
    }
}

#[tokio::test]
async fn reset_and_destroy_refuse_while_any_container_exists() {
    let h = Harness::new();
    let id = ensured(h.ensure("ws-1", 1).await);
    for state in ["running", "exited", "created"] {
        h.fake.set_state(&id, state);
        assert_eq!(
            code(&h.simple(reset("ws-1")).await),
            LauncherErrorCode::Busy
        );
        assert_eq!(
            code(&h.simple(destroy("ws-1")).await),
            LauncherErrorCode::Busy
        );
    }
    assert_eq!(h.fake.count("remove_volume"), 0);
    assert!(proven(&h.stop("ws-1", 1).await));
    let home_labels = h.fake.with(|s| s.volumes["aks-home-ws-1"].labels.clone());
    assert_eq!(h.simple(reset("ws-1")).await, Reply::Done);
    assert_eq!(h.fake.count("remove_volume aks-home-ws-1"), 1);
    assert_eq!(h.fake.count("remove_volume aks-ws-ws-1"), 0);
    assert_eq!(
        h.fake.with(|s| s.volumes["aks-home-ws-1"].labels.clone()),
        home_labels
    );
    ensured(h.ensure("ws-1", 2).await);
    assert!(proven(&h.stop("ws-1", 2).await));
    assert_eq!(h.simple(destroy("ws-1")).await, Reply::Done);
    assert!(h.fake.with(|s| s.volumes.is_empty()));
    assert_eq!(
        std::fs::read_dir(h.root.join("sockets")).unwrap().count(),
        0
    );
    assert_eq!(h.simple(destroy("ws-1")).await, Reply::Done, "idempotent");
    assert_eq!(
        code(&h.ensure("ws-1", 2).await),
        stale(2),
        "the fence stays"
    );
}

#[tokio::test]
async fn usage_reads_both_volumes_from_one_cached_call() {
    let h = Harness::new();
    h.fake.with(|s| {
        s.sizes.insert("aks-ws-ws-1".into(), Some(1_024_000_000));
        s.sizes.insert("aks-home-ws-1".into(), Some(4096));
        s.sizes.insert("unrelated".into(), None);
    });
    let expected = Reply::Usage {
        workspace_bytes: 1_024_000_000,
        home_bytes: 4096,
    };
    assert_eq!(h.simple(usage("ws-1")).await, expected);
    assert_eq!(h.simple(usage("ws-1")).await, expected);
    assert_eq!(
        h.simple(usage("ws-2")).await,
        Reply::Usage {
            workspace_bytes: 0,
            home_bytes: 0
        }
    );
    assert_eq!(h.fake.count("df"), 1, "served from the cache");
    assert_eq!(h.simple(reset("ws-1")).await, Reply::Done);
    h.simple(usage("ws-1")).await;
    assert_eq!(h.fake.count("df"), 2, "reset_home invalidates the cache");
}

#[tokio::test]
async fn an_unknown_volume_size_is_an_error_not_zero() {
    let h = Harness::new();
    h.fake.with(|s| s.down = true);
    assert_eq!(
        code(&h.simple(usage("ws-1")).await),
        LauncherErrorCode::RuntimeFailed
    );
    h.fake.with(|s| {
        s.down = false;
        s.sizes.insert("aks-ws-ws-1".into(), None);
    });
    assert_eq!(
        code(&h.simple(usage("ws-1")).await),
        LauncherErrorCode::RuntimeFailed
    );
}
