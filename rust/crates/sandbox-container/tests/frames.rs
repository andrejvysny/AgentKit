//! Streams longer than one data frame (1 MiB) through the real supervisor:
//! a write sent while its reply is awaited, a read and command output
//! collected from many frames.

mod support;

use std::time::{Duration, Instant};

use agentkit_egress::EgressPorts;
use agentkit_workspaces::{
    ByteSource, FileMode, Generation, MachineHandle, OBSERVE_CHUNK_MAX_BYTES, OperationId,
    OperationKind, OperationRequest, OperationState, OutputLimit, PathRoot, ReadFile, RelPath,
    SandboxBackend, Sha256,
};
use support::{DIGEST, backend_with, embedded, spec};

const MIB: usize = 1024 * 1024;

fn request(m: &MachineHandle, id: &str, kind: OperationKind, args: &[u8]) -> OperationRequest {
    OperationRequest {
        id: OperationId::new(id),
        workspace_id: m.workspace_id.clone(),
        generation: m.generation,
        args_sha256: Sha256::of(args),
        kind,
    }
}

/// Observe `id` to its receipt; answers the state and all output.
async fn finish(
    backend: &dyn SandboxBackend,
    m: &MachineHandle,
    id: &OperationId,
) -> (OperationState, Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut seq, mut output) = (0, Vec::new());
    loop {
        let seen = backend.observe(m, id, seq).await.unwrap();
        assert!(seen.output.len() as u64 <= OBSERVE_CHUNK_MAX_BYTES);
        seq = seen.next_seq;
        output.extend_from_slice(&seen.output);
        if seen.state.is_terminal() && seen.eof {
            return (seen.state, output);
        }
        assert!(Instant::now() < deadline, "{id} has no receipt in time");
        if seen.output.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_reads_and_output_cross_many_data_frames() {
    let dir = tempfile::tempdir().unwrap();
    let starter = embedded::starter(dir.path().join("data"), DIGEST);
    let (_launcher, backend) = backend_with(dir.path(), starter, |_| {}, EgressPorts::system());
    let m = backend.ensure_machine(&spec("ws-1", 1)).await.unwrap();
    assert_eq!(m.generation, Generation(1));

    let bytes: Vec<u8> = (0..3 * MIB + 17).map(|i| b'a' + (i % 26) as u8).collect();
    let path = RelPath::parse("big/file.txt").unwrap();
    let write = OperationKind::WriteFile {
        root: PathRoot::Workspace,
        path: path.clone(),
        sha256: Sha256::of(&bytes),
        bytes: ByteSource::Inline(bytes.clone()),
        mode: FileMode::REGULAR,
    };
    let op = request(&m, "op-write", write, &bytes);
    backend.dispatch(&m, &op).await.unwrap();
    let (state, _) = finish(&backend, &m, &op.id).await;
    assert_eq!(state, OperationState::Succeeded { exit: 0 });

    let read = ReadFile {
        root: PathRoot::Workspace,
        path,
        offset: 0,
        limit_bytes: 10 * MIB as u64,
        max_file_bytes: 10 * MIB as u64,
        raw: false,
    };
    let chunk = backend.read_file(&m, &read).await.unwrap();
    assert!(chunk.eof && !chunk.binary);
    assert_eq!(
        Sha256::of(&chunk.bytes),
        Sha256::of(&bytes),
        "one read, many frames"
    );

    let command = "yes a | head -c 2500000";
    let exec = OperationKind::Exec {
        command: command.to_owned(),
        cwd: RelPath::root(),
        timeout_ms: 60_000,
        output_limit: OutputLimit::LOG,
    };
    let op = request(&m, "op-print", exec, command.as_bytes());
    backend.dispatch(&m, &op).await.unwrap();
    let (state, output) = finish(&backend, &m, &op.id).await;
    assert_eq!(state, OperationState::Succeeded { exit: 0 });
    assert_eq!(output.len(), 2_500_000);
    assert!(output.chunks(2).all(|pair| pair == b"a\n"));
}
