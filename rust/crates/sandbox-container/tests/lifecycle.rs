//! Starting and stopping machines against a fake launcher and a scripted
//! guest: the handshake (channel token, impostors), the one control
//! connection, launcher refusals, stale generations and stop proofs.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentkit_egress::EgressPorts;
use agentkit_sandbox_container::ContainerBackend;
use agentkit_sandbox_proto::PROTOCOL_VERSION;
use agentkit_sandbox_proto::guest::{GuestError, GuestErrorCode, OperationState, Reply, Request};
use agentkit_sandbox_proto::launcher::LauncherErrorCode;
use agentkit_workspaces::{
    EXEC_TIMEOUT_DEFAULT_MS, Generation, ListEntries, NetworkPolicy, OperationId, OperationKind,
    OperationRequest, OutputLimit, PathRoot, ReadFile, RelPath, SandboxBackend, SandboxError,
    Sha256, StopProof, WorkspaceId,
};
use support::guest::{Action, Peer, Script, dial, hello, reply, serve, silent_starter, starter};
use support::launcher::{FakeLauncher, Start, Starter};
use support::{DIGEST, backend_with, spec};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

struct Fixture {
    _dir: TempDir,
    launcher: FakeLauncher,
    backend: ContainerBackend,
}

fn fixture_with(starter: Starter, handshake: Duration) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let tune = |config: &mut agentkit_sandbox_container::ContainerBackendConfig| {
        config.handshake_timeout = handshake;
        config.request_timeout = Duration::from_millis(500);
        config.stop_grace_ms = 500;
    };
    let (launcher, backend) = backend_with(dir.path(), starter, tune, EgressPorts::system());
    Fixture {
        _dir: dir,
        launcher,
        backend,
    }
}

/// A guest that greets honestly, acks every exec as running and answers
/// `shutdown` with `bye`.
fn honest() -> Script {
    Arc::new(|id, peer, request| match request {
        Request::Hello { .. } => hello(id, peer, DIGEST),
        Request::Exec(_) => Action::Send(vec![reply(
            id,
            Reply::Accepted {
                state: OperationState::Running,
                existing: false,
            },
        )]),
        Request::Shutdown => Action::Send(vec![reply(id, Reply::Bye)]),
        _ => Action::Silent,
    })
}

fn fixture(script: Script) -> Fixture {
    fixture_with(starter(script), Duration::from_secs(5))
}

/// What a scripted guest answers to `hello`, from its own machine facts.
type Answer = Arc<dyn Fn(&Peer) -> Reply + Send + Sync>;

/// A `hello` answer that is right but for what `change` alters.
fn hello_but(change: impl Fn(&mut Reply) + Send + Sync + 'static) -> Answer {
    Arc::new(move |peer: &Peer| {
        let mut answer = Reply::Hello {
            protocol: PROTOCOL_VERSION,
            generation: peer.generation,
            image_digest: DIGEST.to_owned(),
            channel_token: peer.token.clone(),
        };
        change(&mut answer);
        answer
    })
}

/// Each wrong `hello` (protocol, generation, image, channel token, a
/// refusal) is refused and the container stopped; the error names the
/// refusal but never the token.
#[tokio::test]
async fn a_bad_hello_is_refused_and_the_container_stopped() {
    let refusal: Answer = Arc::new(|_| {
        Reply::Error(GuestError {
            code: GuestErrorCode::GenerationMismatch,
            message: "another generation".into(),
        })
    });
    let answers: [(&str, Answer); 5] = [
        (
            "protocol",
            hello_but(|r| {
                if let Reply::Hello { protocol, .. } = r {
                    *protocol += 1;
                }
            }),
        ),
        (
            "generation",
            hello_but(|r| {
                if let Reply::Hello { generation, .. } = r {
                    *generation += 1;
                }
            }),
        ),
        (
            "image",
            hello_but(|r| {
                if let Reply::Hello { image_digest, .. } = r {
                    *image_digest = "sha256:another-image".to_owned();
                }
            }),
        ),
        (
            "channel token",
            hello_but(|r| {
                if let Reply::Hello { channel_token, .. } = r {
                    channel_token.push('x');
                }
            }),
        ),
        ("refused", refusal),
    ];
    for (shown, answer) in answers {
        let tokens = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = tokens.clone();
        let script: Script = Arc::new(move |id, peer, request| match request {
            Request::Hello { .. } => {
                seen.lock().unwrap().push(peer.token.clone());
                Action::Send(vec![reply(id, answer(peer))])
            }
            _ => Action::Silent,
        });
        let f = fixture_with(starter(script), Duration::from_millis(500));
        let outcome = f.backend.ensure_machine(&spec("ws-1", 1)).await;
        assert!(
            matches!(outcome, Err(SandboxError::Unavailable { .. })),
            "{shown}: {outcome:?}"
        );
        let text = format!("{outcome:?}");
        assert!(
            text.contains(shown),
            "{shown}: the refusal is named: {text}"
        );
        let token = tokens.lock().unwrap().first().cloned().unwrap();
        assert!(
            !text.contains(&token),
            "{shown}: the token never shows: {text}"
        );
        assert_eq!(
            f.launcher.calls(),
            ["ensure ws-1 1", "stop ws-1 1"],
            "{shown}"
        );
        let again = f.backend.ensure_machine(&spec("ws-1", 1)).await;
        assert!(
            matches!(again, Err(SandboxError::StaleGeneration { .. })),
            "a proven generation never restarts: {again:?}"
        );
    }
}

#[tokio::test]
async fn a_guest_that_never_dials_times_out_and_is_stopped() {
    let f = fixture_with(silent_starter(), Duration::from_millis(300));
    let started = Instant::now();
    let outcome = f.backend.ensure_machine(&spec("ws-1", 1)).await;
    assert!(
        matches!(outcome, Err(SandboxError::Unavailable { .. })),
        "{outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the handshake is bounded"
    );
    assert_eq!(f.launcher.calls(), ["ensure ws-1 1", "stop ws-1 1"]);
}

#[tokio::test]
async fn only_the_first_control_connection_is_accepted() {
    let f = fixture(honest());
    let m = f.backend.ensure_machine(&spec("ws-1", 1)).await.unwrap();
    assert_eq!(f.backend.ensure_machine(&spec("ws-1", 1)).await.unwrap(), m);
    let dir = f.launcher.machine_dir("ws-1", 1);
    for name in ["control.sock", "egress.sock"] {
        let mode = std::fs::metadata(dir.join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o666, "{name}");
    }
    let second = UnixStream::connect(dir.join("control.sock")).await;
    assert!(second.is_err(), "a second control connection is refused");
}

/// A starter where other peers reach the control socket before the guest:
/// `impostors` connections that answer `hello` with `token` and nothing
/// else (or, when `token` is `None`, stay silent), then the honest guest.
/// Each silent impostor's stream is handed to `held` so a test can see the
/// adapter drop it.
fn impostors_first(
    impostors: usize,
    token: Option<&'static str>,
    held: Arc<Mutex<Vec<UnixStream>>>,
) -> Starter {
    Arc::new(move |start: Start| {
        let held = held.clone();
        tokio::spawn(async move {
            let control = start.dir.join("control.sock");
            let peer = Peer {
                generation: start.generation,
                token: start.channel_token.clone(),
            };
            for _ in 0..impostors {
                let stream = dial(&control).await.unwrap();
                match token {
                    Some(token) => {
                        let fake = Peer {
                            token: token.to_owned(),
                            ..peer.clone()
                        };
                        let greets_only: Script = Arc::new(|id, peer, request| match request {
                            Request::Hello { .. } => hello(id, peer, DIGEST),
                            _ => Action::Silent,
                        });
                        tokio::spawn(async move { serve(stream, &fake, &greets_only).await });
                    }
                    None => held.lock().unwrap().push(stream),
                }
            }
            if let Some(stream) = dial(&control).await {
                serve(stream, &peer, &honest()).await;
            }
        })
    })
}

/// A peer that reaches the control socket first and stays silent cannot
/// hold the guest out, and one that answers the wrong token is refused;
/// the guest still gets in, and the failed peers are dropped.
#[tokio::test]
async fn impostors_that_connect_first_are_refused_and_the_guest_gets_in() {
    for token in [None, Some("not-the-token")] {
        let held = Arc::new(Mutex::new(Vec::new()));
        let f = fixture_with(
            impostors_first(3, token, held.clone()),
            Duration::from_secs(5),
        );
        let started = Instant::now();
        let m = f.backend.ensure_machine(&spec("ws-1", 1)).await;
        let m = m.unwrap_or_else(|error| panic!("{token:?}: {error:?}"));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{token:?}: no wait on a silent peer"
        );
        assert_eq!(f.launcher.calls(), ["ensure ws-1 1"], "{token:?}");
        let dispatched = f.backend.dispatch(&m, &exec(1, "op-1")).await.unwrap();
        assert!(
            !dispatched.existing,
            "{token:?}: the guest serves the machine"
        );
        let streams: Vec<UnixStream> = std::mem::take(&mut *held.lock().unwrap());
        for mut stream in streams {
            // It got a `hello` it never answered; then the adapter closed it.
            let mut sent = Vec::new();
            let read =
                tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut sent)).await;
            assert!(read.is_ok(), "the silent peer is dropped");
            assert!(!sent.is_empty(), "the silent peer was greeted");
        }
        let dir = f.launcher.machine_dir("ws-1", 1);
        assert!(UnixStream::connect(dir.join("control.sock")).await.is_err());
    }
}

/// At most `HANDSHAKE_ATTEMPTS` (16) connections are taken: a flood of
/// wrong peers ends the start, which stops the container.
#[tokio::test]
async fn the_handshake_takes_a_bounded_number_of_connections() {
    let held = Arc::new(Mutex::new(Vec::new()));
    let f = fixture_with(
        impostors_first(16, Some("not-the-token"), held),
        Duration::from_secs(5),
    );
    let started = Instant::now();
    let outcome = f.backend.ensure_machine(&spec("ws-1", 1)).await;
    match &outcome {
        Err(SandboxError::Unavailable { reason }) => {
            assert!(reason.contains("16 control connections"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "no wait for the deadline"
    );
    assert_eq!(f.launcher.calls(), ["ensure ws-1 1", "stop ws-1 1"]);
}

#[tokio::test]
async fn invalid_workspace_ids_never_reach_the_launcher() {
    let f = fixture(honest());
    let bad = WorkspaceId::new("WS_1/../x");
    let mut bad_spec = spec("ws-1", 1);
    bad_spec.workspace_id = bad.clone();
    let outcome = f.backend.ensure_machine(&bad_spec).await;
    assert!(
        matches!(outcome, Err(SandboxError::Backend { .. })),
        "{outcome:?}"
    );
    assert!(f.backend.stop_machine(&bad, Generation(1)).await.is_err());
    assert!(f.backend.destroy_storage(&bad).await.is_err());
    assert!(f.launcher.calls().is_empty());
}

#[tokio::test]
async fn launcher_refusals_become_port_errors() {
    let f = fixture(honest());
    f.launcher
        .fail_next_ensure(LauncherErrorCode::ImageMismatch);
    let mismatch = f.backend.ensure_machine(&spec("ws-1", 1)).await;
    assert!(
        matches!(mismatch, Err(SandboxError::Unavailable { .. })),
        "{mismatch:?}"
    );
    f.launcher
        .fail_next_ensure(LauncherErrorCode::StaleGeneration { current: 5 });
    let stale = f.backend.ensure_machine(&spec("ws-1", 1)).await;
    assert_eq!(
        stale,
        Err(SandboxError::StaleGeneration {
            requested: Generation(1),
            current: Generation(5)
        })
    );
}

#[tokio::test]
async fn a_pinned_digest_refuses_other_images_without_the_launcher() {
    let dir = tempfile::tempdir().unwrap();
    let tune = |config: &mut agentkit_sandbox_container::ContainerBackendConfig| {
        config.image_digest = Some("sha256:installed".to_owned());
    };
    let ports = EgressPorts::system();
    let (launcher, backend) = backend_with(dir.path(), starter(honest()), tune, ports);
    let outcome = backend.ensure_machine(&spec("ws-1", 1)).await;
    assert!(
        matches!(outcome, Err(SandboxError::Unavailable { .. })),
        "{outcome:?}"
    );
    assert!(launcher.calls().is_empty());
}

/// A stop of a generation covers every lower one, here as at the
/// launcher: the live machine of a lower generation is shut down and
/// forgotten, so the next generation starts.
#[tokio::test]
async fn a_stop_takes_a_live_machine_of_a_lower_generation() {
    let f = fixture(honest());
    let ws = WorkspaceId::new("ws-1");
    let old = f.backend.ensure_machine(&spec("ws-1", 1)).await.unwrap();
    assert_eq!(
        f.backend.stop_machine(&ws, Generation(2)).await,
        Ok(StopProof::Stopped)
    );
    let unavailable = f.backend.dispatch(&old, &exec(1, "op-1")).await;
    assert!(
        matches!(unavailable, Err(SandboxError::Unavailable { .. })),
        "{unavailable:?}"
    );
    let next = f.backend.ensure_machine(&spec("ws-1", 3)).await.unwrap();
    assert_eq!(next.generation, Generation(3));
    assert_eq!(
        f.launcher.calls(),
        ["ensure ws-1 1", "stop ws-1 2", "ensure ws-1 3"]
    );
}

#[tokio::test]
async fn stop_shuts_the_guest_down_and_repeats() {
    let f = fixture(honest());
    let ws = WorkspaceId::new("ws-1");
    f.backend.ensure_machine(&spec("ws-1", 1)).await.unwrap();
    let control = f.launcher.machine_dir("ws-1", 1).join("control.sock");
    assert!(control.exists());
    assert_eq!(
        f.backend.stop_machine(&ws, Generation(1)).await,
        Ok(StopProof::Stopped)
    );
    assert!(
        !control.exists(),
        "the control socket is removed at the stop"
    );
    assert_eq!(
        f.backend.stop_machine(&ws, Generation(1)).await,
        Ok(StopProof::Stopped)
    );
    assert_eq!(
        f.launcher.calls(),
        ["ensure ws-1 1", "stop ws-1 1", "stop ws-1 1"]
    );
}

fn exec(m_generation: u64, id: &str) -> OperationRequest {
    let command = "printf hi";
    OperationRequest {
        id: OperationId::new(id),
        workspace_id: WorkspaceId::new("ws-1"),
        generation: Generation(m_generation),
        args_sha256: Sha256::of(command.as_bytes()),
        kind: OperationKind::Exec {
            command: command.to_owned(),
            cwd: RelPath::root(),
            timeout_ms: EXEC_TIMEOUT_DEFAULT_MS,
            output_limit: OutputLimit::LOG,
        },
    }
}

fn assert_stale<T: std::fmt::Debug>(outcome: Result<T, SandboxError>) {
    match outcome {
        Err(SandboxError::StaleGeneration { requested, current }) => {
            assert_eq!((requested, current), (Generation(1), Generation(2)));
        }
        other => panic!("expected stale 1 < 2, got {other:?}"),
    }
}

#[tokio::test]
async fn an_old_generation_is_stale_everywhere() {
    let f = fixture(honest());
    let (backend, ws) = (&f.backend, WorkspaceId::new("ws-1"));
    let old = backend.ensure_machine(&spec("ws-1", 1)).await.unwrap();
    assert_eq!(
        backend.stop_machine(&ws, Generation(1)).await,
        Ok(StopProof::Stopped)
    );
    let new = backend.ensure_machine(&spec("ws-1", 2)).await.unwrap();
    let id = OperationId::new("op-1");
    let read = ReadFile {
        root: PathRoot::Workspace,
        path: RelPath::parse("a.txt").unwrap(),
        offset: 0,
        limit_bytes: 10,
        max_file_bytes: 10,
        raw: false,
    };
    let list = ListEntries {
        root: PathRoot::Workspace,
        path: RelPath::root(),
        depth: 1,
        max_entries: 10,
    };
    assert_stale(backend.ensure_machine(&spec("ws-1", 1)).await);
    assert_stale(backend.dispatch(&old, &exec(1, "op-1")).await);
    assert_stale(backend.dispatch(&new, &exec(1, "op-1")).await);
    assert_stale(backend.observe(&old, &id, 0).await);
    assert_stale(backend.cancel(&old, &id).await);
    assert_stale(backend.read_file(&old, &read).await);
    assert_stale(backend.list(&old, &list).await);
    let open = NetworkPolicy::default();
    assert_stale(backend.apply_network_policy(&old, &open).await);
    assert_stale(backend.drain_denials(&old, 0).await);
}
