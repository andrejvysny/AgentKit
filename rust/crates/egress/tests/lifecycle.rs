//! Binding, the denial cursor (NET-5) and shutdown.

mod support;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::sync::Arc;
use std::time::Duration;

use agentkit_egress::{EgressConfig, EgressError, EgressPorts, EgressProxy};
use support::{closes_within, config, echo_upstream, greet, read_all, round_trip, start, target};
use tokio::net::UnixStream;

#[tokio::test]
async fn the_socket_is_connectable_by_anyone_who_can_reach_its_directory() {
    let h = start(config());
    let meta = std::fs::symlink_metadata(h.proxy.path()).expect("socket exists");
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o666);
}

#[tokio::test]
async fn bind_refuses_an_existing_path_and_a_zero_bound() {
    let h = start(config());
    let taken = EgressProxy::bind(h.proxy.path(), config(), EgressPorts::system());
    assert!(matches!(taken, Err(EgressError::Bind(_))), "{taken:?}");
    let file = h.dir.path().join("plain-file");
    std::fs::write(&file, b"x").expect("write");
    let taken = EgressProxy::bind(&file, config(), EgressPorts::system());
    assert!(matches!(taken, Err(EgressError::Bind(_))), "{taken:?}");
    assert_eq!(std::fs::read(&file).expect("kept"), b"x");

    let zero = EgressConfig {
        max_tunnels: 0,
        ..config()
    };
    let path = h.dir.path().join("zero.sock");
    let refused = EgressProxy::bind(&path, zero, EgressPorts::system());
    assert!(
        matches!(refused, Err(EgressError::InvalidConfig(_))),
        "{refused:?}"
    );
    assert!(!path.exists(), "nothing bound for an invalid config");
}

#[tokio::test]
async fn drain_reads_by_cursor_and_survives_overflow() {
    let h = start(EgressConfig {
        denial_buffer: 3,
        ..config()
    });
    for (i, host) in ["a.test", "b.test", "c.test", "d.test", "e.test"]
        .iter()
        .enumerate()
    {
        h.proxy
            .set_current_operation((i % 2 == 0).then(|| format!("op-{i}")));
        let (mut client, _) = h.connect(&format!("{host}:443")).await;
        read_all(&mut client).await;
    }
    let batch = h.proxy.drain(0);
    let seen: Vec<(u64, &str, Option<&str>)> = batch
        .denials
        .iter()
        .map(|d| (d.seq, d.host.as_str(), d.operation_id.as_deref()))
        .collect();
    assert_eq!(
        seen,
        [
            (3, "c.test", Some("op-2")),
            (4, "d.test", None),
            (5, "e.test", Some("op-4"))
        ]
    );
    assert_eq!((batch.cursor, batch.missed), (5, 2));

    let later = h.proxy.drain(4);
    assert_eq!((later.denials.len(), later.cursor, later.missed), (1, 5, 0));
    let none = h.proxy.drain(5);
    assert_eq!((none.denials.len(), none.cursor), (0, 5));
    let ahead = h.proxy.drain(99);
    assert_eq!(
        (ahead.denials.len(), ahead.cursor),
        (0, 5),
        "a foreign cursor is clamped"
    );
    assert_eq!(h.proxy.stats().denials, 5);
}

#[tokio::test]
async fn shutdown_closes_tunnels_and_removes_the_socket() {
    let h = start(config());
    h.route_name("example.test", 443, echo_upstream().await);
    h.proxy.set_policy(vec![target("example.test", 443)]);
    let (mut tunnel, _) = h.connect("example.test:443").await;
    greet(&mut tunnel, Some("example.test")).await;
    assert_eq!(round_trip(&mut tunnel, b"x").await, b"x");
    let mut waiting = h.client().await;
    let path = h.proxy.path().to_owned();

    support::within(h.proxy.shutdown()).await;
    assert!(!path.exists(), "socket file removed");
    assert!(closes_within(&mut tunnel, Duration::from_secs(1)).await);
    assert!(closes_within(&mut waiting, Duration::from_secs(1)).await);
    assert!(UnixStream::connect(&path).await.is_err());
}

#[tokio::test]
async fn dropping_the_proxy_stops_it() {
    let h = start(config());
    let path = h.proxy.path().to_owned();
    let mut waiting = h.client().await;
    drop(h.proxy);
    assert!(!path.exists());
    assert!(closes_within(&mut waiting, Duration::from_secs(1)).await);
}

#[tokio::test]
async fn shutdown_leaves_a_socket_it_does_not_own() {
    let h = start(config());
    let path = h.proxy.path().to_owned();
    std::fs::remove_file(&path).expect("remove");
    let ports = EgressPorts {
        runtime: Arc::new(agentkit_kernel::SystemRuntime),
        ..EgressPorts::system()
    };
    let other = EgressProxy::bind(&path, config(), ports).expect("rebind");
    support::within(h.proxy.shutdown()).await;
    assert!(path.exists(), "the newer proxy's socket stays");
    drop(other);
    assert!(!path.exists());
}
