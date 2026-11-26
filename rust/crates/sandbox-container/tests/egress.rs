//! The machine's egress proxy: grants from the spec and from a live
//! policy change, denials stamped with the operation dispatched last, and
//! `limit` refusals left out of the drain (NET-5, NET-8).

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentkit_sandbox_container::ContainerBackendConfig;
use agentkit_sandbox_proto::guest::{OperationState, Reply, Request};
use agentkit_workspaces::{
    Denial, EXEC_TIMEOUT_DEFAULT_MS, Generation, HostPort, NetworkPolicy, OperationId,
    OperationKind, OperationRequest, OutputLimit, RelPath, SandboxBackend, Sha256, WorkspaceId,
};
use support::guest::{Action, Script, hello, reply, starter};
use support::{DIGEST, backend_with, local_ports, spec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};

/// An upstream that accepts and holds every connection.
async fn holding_upstream() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    addr
}

/// Send a CONNECT for `target` and read the response head.
async fn connect(socket: &Path, target: &str) -> (UnixStream, String) {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    while !answer.ends_with(b"\r\n\r\n") {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("the proxy answers")
            .unwrap();
        assert_eq!(read, 1, "the proxy closed before its answer");
        answer.push(byte[0]);
    }
    (stream, String::from_utf8(answer).unwrap())
}

fn exec(id: &str) -> OperationRequest {
    OperationRequest {
        id: OperationId::new(id),
        workspace_id: WorkspaceId::new("ws-1"),
        generation: Generation(1),
        args_sha256: Sha256::of(b"curl"),
        kind: OperationKind::Exec {
            command: "curl".to_owned(),
            cwd: RelPath::root(),
            timeout_ms: EXEC_TIMEOUT_DEFAULT_MS,
            output_limit: OutputLimit::LOG,
        },
    }
}

fn running_guest() -> Script {
    Arc::new(|id, peer, request| match request {
        Request::Hello { .. } => hello(id, peer, DIGEST),
        Request::Exec(_) => Action::Send(vec![reply(
            id,
            Reply::Accepted {
                state: OperationState::Running,
                existing: false,
            },
        )]),
        _ => Action::Silent,
    })
}

#[tokio::test]
async fn limit_refusals_are_not_drained_and_denials_name_the_operation() {
    let dir = tempfile::tempdir().unwrap();
    let tune = |config: &mut ContainerBackendConfig| config.egress.max_tunnels = 1;
    let ports = local_ports(holding_upstream().await);
    let (launcher, backend) = backend_with(dir.path(), starter(running_guest()), tune, ports);
    let mut machine_spec = spec("ws-1", 1);
    machine_spec.network = NetworkPolicy {
        grants: vec![HostPort::new("pypi.org", 443).unwrap()],
    };
    let m = backend.ensure_machine(&machine_spec).await.unwrap();
    backend.dispatch(&m, &exec("op-1")).await.unwrap();
    let socket = launcher.machine_dir("ws-1", 1).join("egress.sock");
    let (_tunnel, head) = connect(&socket, "pypi.org:443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let (_full, head) = connect(&socket, "pypi.org:443").await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    let (_refused, head) = connect(&socket, "example.com:443").await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    let batch = backend.drain_denials(&m, 0).await.unwrap();
    let hosts: Vec<(&str, u16, Option<&str>)> = batch
        .denials
        .iter()
        .map(|d: &Denial| {
            (
                d.host.as_str(),
                d.port,
                d.operation_id.as_ref().map(OperationId::as_str),
            )
        })
        .collect();
    assert_eq!(hosts, [("example.com", 443, Some("op-1"))]);
    assert_eq!(
        batch.cursor, 2,
        "the cursor passes the skipped limit refusal"
    );
    let again = backend.drain_denials(&m, batch.cursor).await.unwrap();
    assert!(again.denials.is_empty());
    assert_eq!(again.cursor, 2);
}

#[tokio::test]
async fn a_live_policy_change_applies_to_new_connections() {
    let dir = tempfile::tempdir().unwrap();
    let ports = local_ports(holding_upstream().await);
    let (launcher, backend) = backend_with(dir.path(), starter(running_guest()), |_| {}, ports);
    let m = backend.ensure_machine(&spec("ws-1", 1)).await.unwrap();
    let socket = launcher.machine_dir("ws-1", 1).join("egress.sock");
    let (_refused, head) = connect(&socket, "pypi.org:443").await;
    assert!(
        head.starts_with("HTTP/1.1 403"),
        "nothing is granted yet: {head}"
    );
    let open = NetworkPolicy {
        grants: vec![HostPort::new("pypi.org", 443).unwrap()],
    };
    backend.apply_network_policy(&m, &open).await.unwrap();
    let (_tunnel, head) = connect(&socket, "pypi.org:443").await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let batch = backend.drain_denials(&m, 0).await.unwrap();
    assert_eq!(batch.denials.len(), 1);
    assert_eq!(batch.denials[0].operation_id, None, "no operation ran");
}
