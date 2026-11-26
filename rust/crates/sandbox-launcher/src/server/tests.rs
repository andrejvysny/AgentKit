use std::collections::BTreeMap;

use agentkit_sandbox_proto::launcher::{ReplyEnvelope, RequestEnvelope, Tier};
use agentkit_sandbox_proto::{encode_data, parse_control};
use tokio::io::AsyncWriteExt;

use super::*;
use crate::config::Config;
use crate::fake::{DIGEST, FakeRuntime, scratch, test_config};
use crate::runtime::RuntimeApi;
use crate::state::StateStore;
use crate::trust::TrustRoot;

fn service(config: &Config) -> Arc<Service> {
    let runtime: Arc<dyn RuntimeApi> = Arc::new(FakeRuntime::default());
    let state = StateStore::open(&config.state_dir).unwrap();
    let trust = TrustRoot::at(config.state_dir.parent().unwrap());
    Arc::new(Service::new(
        Arc::new(config.clone()),
        runtime,
        state,
        trust,
    ))
}

/// Start the accept loop on the configured socket; the test process is
/// the server uid.
async fn listen(config: &Config) -> UnixStream {
    let listener = UnixListener::bind(&config.listen_socket).unwrap();
    let peers = Peers {
        server_uid: config.server_uid,
        own_uid: config.server_uid,
    };
    tokio::spawn(serve(listener, service(config), peers, 4));
    UnixStream::connect(&config.listen_socket).await.unwrap()
}

fn ensure(id: u64) -> Vec<u8> {
    let request = Request::Ensure {
        workspace_id: "ws-1".into(),
        generation: 1,
        image_digest: DIGEST.into(),
        tier: Tier {
            cpus_milli: 2000,
            memory_bytes: 2 << 30,
            pids: 512,
        },
    };
    encode_control(&RequestEnvelope::new(id, request)).unwrap()
}

fn inspect(id: u64) -> Vec<u8> {
    let request = Request::Inspect {
        workspace_id: "ws-1".into(),
    };
    encode_control(&RequestEnvelope::new(id, request)).unwrap()
}

/// Read replies until `count` arrived or the server closed the stream.
async fn replies(stream: UnixStream, count: usize) -> BTreeMap<u64, Reply> {
    let (read, _write) = stream.into_split();
    let mut reader = FrameReader::new(read);
    let mut out = BTreeMap::new();
    while out.len() < count {
        match reader.next().await {
            Ok(Some(Frame::Control(json))) => {
                let envelope: ReplyEnvelope = parse_control(&json).unwrap();
                out.insert(envelope.id, envelope.body);
            }
            Ok(Some(Frame::Data(_))) => panic!("the launcher sends control frames only"),
            // Closed, or reset because the request was never read.
            Ok(None) | Err(_) => break,
        }
    }
    out
}

#[tokio::test]
async fn requests_round_trip_with_their_ids() {
    let (_tmp, root) = scratch();
    let config = test_config(&root);
    let mut client = listen(&config).await;
    client.write_all(&ensure(7)).await.unwrap();
    let got = replies(client, 1).await;
    assert!(matches!(got[&7], Reply::Ensured { .. }), "{got:?}");

    let mut client = UnixStream::connect(&config.listen_socket).await.unwrap();
    client.write_all(&inspect(9)).await.unwrap();
    client.write_all(&ensure(8)).await.unwrap();
    let got = replies(client, 2).await;
    assert!(matches!(got[&8], Reply::Ensured { .. }), "{got:?}");
    assert!(matches!(got[&9], Reply::Inspected { .. }), "{got:?}");
}

#[tokio::test]
async fn a_bad_body_with_an_id_is_answered_and_the_connection_stays() {
    let (_tmp, root) = scratch();
    let config = test_config(&root);
    let mut client = listen(&config).await;
    let bad = r#"{"id":3,"body":{"type":"run","image":"evil"}}"#;
    let frame = agentkit_sandbox_proto::Frame::Control(bad.into());
    client.write_all(&frame.encode().unwrap()).await.unwrap();
    client.write_all(&inspect(4)).await.unwrap();
    let got = replies(client, 2).await;
    match &got[&3] {
        Reply::Error(error) => assert_eq!(error.code, LauncherErrorCode::InvalidRequest),
        other => panic!("{other:?}"),
    }
    assert!(matches!(got[&4], Reply::Inspected { .. }), "{got:?}");
}

#[tokio::test]
async fn protocol_breaks_close_the_connection() {
    let (_tmp, root) = scratch();
    let config = test_config(&root);
    let breaks = [
        encode_data(1, 0, b"x").unwrap(),
        agentkit_sandbox_proto::Frame::Control(r#"{"body":{}}"#.into())
            .encode()
            .unwrap(),
        vec![0, 0, 0, 1, 9],
    ];
    let mut first = true;
    for bytes in breaks {
        let mut client = if first {
            first = false;
            listen(&config).await
        } else {
            UnixStream::connect(&config.listen_socket).await.unwrap()
        };
        client.write_all(&bytes).await.unwrap();
        client.write_all(&inspect(5)).await.unwrap();
        assert!(replies(client, 1).await.is_empty(), "{bytes:?}");
    }
}

#[tokio::test]
async fn another_user_is_refused() {
    let (_tmp, root) = scratch();
    let config = test_config(&root);
    let listener = UnixListener::bind(&config.listen_socket).unwrap();
    let peers = Peers {
        server_uid: config.server_uid.wrapping_add(1),
        own_uid: config.server_uid.wrapping_add(2),
    };
    tokio::spawn(serve(listener, service(&config), peers, 4));
    let mut client = UnixStream::connect(&config.listen_socket).await.unwrap();
    let _ = client.write_all(&inspect(1)).await;
    assert!(replies(client, 1).await.is_empty());
}
