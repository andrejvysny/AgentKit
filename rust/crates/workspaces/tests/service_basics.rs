//! The workspace service on the happy path and its plain refusals:
//! receipts and attribution, paths and roots, copy-in, export reads,
//! status, bounded output, membership (the network request flow is in
//! `service_network.rs`).

mod support;

use agentkit_workspaces::{
    AgentId, Binding, ExecRequest, FileMode, FileRef, OperationPhase, OperationState, OutputLimit,
    PathRoot, RelPath, SandboxError, Sha256, WorkspaceKey, WorkspaceKind, WorkspaceRepository,
    WorkspaceState, WriteRequest,
};
use support::{
    BACKGROUND, ECHO, EXIT_3, IMAGE, SLEEP, SUCCEEDED, TRUE, World, YES, chat, ctx, exec, home,
    rel, settled,
};

fn write(path: &str, bytes: &[u8]) -> WriteRequest {
    WriteRequest {
        file: FileRef::workspace(rel(path)),
        bytes: bytes.to_vec(),
        mode: FileMode::REGULAR,
    }
}

#[tokio::test]
async fn exec_reports_the_receipt_output_and_attribution() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    let done = svc
        .exec(&ctx("op-1", "a1"), &binding, exec(ECHO))
        .await
        .unwrap();
    assert_eq!(settled(&done), SUCCEEDED);
    assert_eq!(done.output, b"hi\n");
    assert!(!done.output_cut && !done.log_truncated && !done.replayed);
    assert_eq!(done.log_path, Some(rel(".sandbox/logs/op-1.log")));
    assert_eq!((done.generation.0, done.image_digest.as_str()), (1, IMAGE));
    let row = world
        .repo
        .operation(&done.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.phase, OperationPhase::Terminal(SUCCEEDED));
    assert_eq!(
        (
            row.actor_agent_id.as_str(),
            row.run_id.as_str(),
            row.summary.as_str()
        ),
        ("a1", "run-a1", ECHO)
    );
    assert_eq!((row.image_digest.as_str(), row.generation.0), (IMAGE, 1));
    let ws = world.workspace(&binding).await;
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert!(
        world
            .repo
            .lease(&ws.id, world.now())
            .await
            .unwrap()
            .is_none()
    );
    let failed = svc
        .exec(&ctx("op-2", "a1"), &binding, exec(EXIT_3))
        .await
        .unwrap();
    let exit = OperationState::Failed {
        exit: Some(3),
        signal: None,
    };
    assert_eq!(settled(&failed), exit);
}

#[tokio::test]
async fn deadline_ends_a_command_as_timed_out() {
    let world = World::new();
    let (svc, _) = world.service();
    let request = ExecRequest {
        timeout_ms: Some(500),
        ..exec(SLEEP)
    };
    let done = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), request)
        .await
        .unwrap();
    assert_eq!(settled(&done), OperationState::TimedOut);
}

/// Relative paths resolve in the chat's folder below `/workspace`; the
/// `Home` root is `/home/agent` and ignores the folder; nothing escapes.
#[tokio::test]
async fn paths_resolve_in_the_chat_folder_or_below_home() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = chat("p1", "c1");
    let note = svc
        .write_file(
            &ctx("op-1", "a1"),
            &binding,
            write("notes/../a.md", b"note"),
        )
        .await
        .unwrap();
    assert_eq!(settled(&note), SUCCEEDED);
    assert_eq!(note.written, Some(FileRef::workspace(rel("chats/c1/a.md"))));
    let home_file = WriteRequest {
        file: FileRef {
            root: PathRoot::Home,
            path: rel(".config/tool.toml"),
        },
        ..write("unused", b"home")
    };
    svc.write_file(&ctx("op-2", "a1"), &binding, home_file)
        .await
        .unwrap();
    let ws = world.workspace(&binding).await;
    assert_eq!(
        world.fake.volume_file(&ws.id, "chats/c1/a.md"),
        Some(b"note".to_vec())
    );
    assert_eq!(
        world.fake.home_file(&ws.id, ".config/tool.toml"),
        Some(b"home".to_vec())
    );
    assert_eq!(
        world.fake.volume_file(&ws.id, "chats/c1/.config/tool.toml"),
        None
    );
    let row = world
        .repo
        .operation(&note.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.summary, "/workspace/chats/c1/a.md");
    // A path that climbs above its root cannot even be named.
    assert!(RelPath::parse("../c2/a.md").is_err());
    assert!(RelPath::parse("a/../../x").is_err());
}

/// Reads resolve in the chat's folder too; listed paths name the folder.
#[tokio::test]
async fn reads_resolve_in_the_chat_folder() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = chat("p1", "c1");
    svc.write_file(&ctx("op-1", "a1"), &binding, write("a.md", b"note"))
        .await
        .unwrap();
    let chunk = svc
        .read_file(
            &binding,
            &AgentId::new("a1"),
            &FileRef::workspace(rel("a.md")),
            0,
            100,
        )
        .await
        .unwrap();
    assert_eq!(chunk.bytes, b"note");
    let listing = svc
        .list(
            &binding,
            &AgentId::new("a1"),
            &FileRef::workspace(RelPath::root()),
            1,
        )
        .await
        .unwrap();
    let paths: Vec<_> = listing.entries.iter().map(|e| e.path.to_string()).collect();
    assert_eq!(paths, ["chats/c1/a.md"]);
}

#[tokio::test]
async fn oversized_writes_are_refused_before_anything_exists() {
    let world = World::new();
    let (svc, _) = world.service();
    let big = vec![b'x'; 5 * 1024 * 1024 + 1];
    let refused = svc
        .write_file(&ctx("op-1", "a1"), &home("a1"), write("big", &big))
        .await;
    assert!(
        matches!(refused, Err(SandboxError::FileTooLarge { .. })),
        "{refused:?}"
    );
    assert!(world.repo.list_workspaces().await.unwrap().is_empty());
}

/// Copy-in lands under `<folder>/input/`, numbers a taken name, and arms
/// the disclosure gate.
#[tokio::test]
async fn copy_in_numbers_collisions_and_marks_user_files() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = chat("p1", "c1");
    let first = svc
        .copy_in(&ctx("op-1", "a1"), &binding, "report.pdf", b"one".to_vec())
        .await
        .unwrap();
    let second = svc
        .copy_in(&ctx("op-2", "a1"), &binding, "report.pdf", b"two".to_vec())
        .await
        .unwrap();
    assert_eq!(
        first.written,
        Some(FileRef::workspace(rel("chats/c1/input/report.pdf")))
    );
    assert_eq!(
        second.written,
        Some(FileRef::workspace(rel("chats/c1/input/report-1.pdf")))
    );
    let ws = world.workspace(&binding).await;
    assert!(ws.holds_user_files);
    assert_eq!(
        world
            .fake
            .volume_file(&ws.id, "chats/c1/input/report-1.pdf"),
        Some(b"two".to_vec())
    );
    let nested = svc
        .copy_in(&ctx("op-3", "a1"), &binding, "a/b.txt", b"x".to_vec())
        .await;
    assert!(
        matches!(nested, Err(SandboxError::PathOutsideWorkspace { .. })),
        "{nested:?}"
    );
}

#[tokio::test]
async fn export_reads_whole_regular_files_binary_included() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = home("a1");
    let agent = AgentId::new("a1");
    let bytes = [0_u8, 159, 146, 150, 0, b'z'];
    svc.write_file(&ctx("op-1", "a1"), &binding, write("out/chart.png", &bytes))
        .await
        .unwrap();
    let file = FileRef::workspace(rel("out/chart.png"));
    let export = svc.read_for_export(&binding, &agent, &file).await.unwrap();
    assert_eq!((export.bytes.as_slice(), export.size), (&bytes[..], 6));
    assert_eq!(export.sha256, Sha256::of(&bytes));
    let dir = svc
        .read_for_export(&binding, &agent, &FileRef::workspace(rel("out")))
        .await;
    assert!(
        matches!(dir, Err(SandboxError::NotARegularFile { .. })),
        "{dir:?}"
    );
    let root = svc
        .read_for_export(&binding, &agent, &FileRef::workspace(RelPath::root()))
        .await;
    assert!(
        matches!(root, Err(SandboxError::NotARegularFile { .. })),
        "{root:?}"
    );
    let missing = svc
        .read_for_export(&binding, &agent, &FileRef::workspace(rel("out/none")))
        .await;
    assert!(
        matches!(missing, Err(SandboxError::NotFound { .. })),
        "{missing:?}"
    );
}

/// F-16: a binding that never ran a mutating call leaves nothing behind;
/// reads and status on it create nothing.
#[tokio::test]
async fn reads_and_status_on_a_never_used_binding_create_nothing() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = chat("p1", "c1");
    let agent = AgentId::new("a1");
    let status = svc.status(&binding, &agent).await.unwrap();
    assert!(status.workspace.is_none() && status.running.is_none());
    assert_eq!(status.working_dir, "/workspace/chats/c1");
    let root = FileRef::workspace(RelPath::root());
    assert!(
        svc.list(&binding, &agent, &root, 2)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let read = svc
        .read_file(&binding, &agent, &FileRef::workspace(rel("a.txt")), 0, 10)
        .await;
    assert!(
        matches!(read, Err(SandboxError::NotFound { .. })),
        "{read:?}"
    );
    assert!(world.repo.list_workspaces().await.unwrap().is_empty());
    assert_eq!(world.fake.machines_started(), 0);
    // After first use the status shows the record and its default grants.
    svc.exec(&ctx("op-1", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    let status = svc.status(&binding, &agent).await.unwrap();
    let ws = status.workspace.expect("created at first use");
    assert_eq!(
        (ws.kind, ws.image_digest.as_str()),
        (WorkspaceKind::ProjectChats, IMAGE)
    );
    assert_eq!(status.grants.len(), 3);
    assert!(status.storage.is_some() && status.pending_requests.is_empty());
}

/// F-12, INV-9: a command printing without end stops at its log limit and
/// the caller sees a bounded tail.
#[tokio::test]
async fn endless_output_is_bounded_for_the_caller() {
    let world = World::new();
    let (svc, _) = world.service();
    let request = ExecRequest {
        output_limit: OutputLimit {
            max_bytes: 1024 * 1024,
            max_lines: None,
        },
        ..exec(YES)
    };
    let done = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), request)
        .await
        .unwrap();
    assert_eq!(settled(&done), OperationState::OutputLimit);
    let model = world.config.model_output;
    assert!(done.output.len() as u64 <= model.max_bytes);
    let lines = done.output.iter().filter(|&&b| b == b'\n').count() as u64;
    assert!(lines <= model.max_lines.unwrap());
    assert!(done.output_cut && done.log_truncated);
    assert!(done.output.ends_with(b"y\n"));
}

/// F-7: a process left behind ends with its command.
#[tokio::test]
async fn a_background_process_ends_with_its_command() {
    let world = World::new();
    let (svc, _) = world.service();
    let done = svc
        .exec(&ctx("op-1", "a1"), &home("a1"), exec(BACKGROUND))
        .await
        .unwrap();
    assert_eq!(settled(&done), SUCCEEDED);
    assert_eq!(world.fake.background_ended(), 1);
}

fn shared(key: &str) -> Binding {
    Binding {
        key: WorkspaceKey::new(key),
        kind: WorkspaceKind::Shared,
        owner_scope: "owner:1".into(),
        folder: None,
    }
}

/// A shared workspace admits members only; a binding of another scope or
/// kind never reaches a stored record (F-18 is the host's; this refuses a
/// forged one anyway); the reserved host-folder kind is not built.
#[tokio::test]
async fn membership_scope_and_kind_are_checked() {
    let world = World::new();
    let (svc, _) = world.service();
    let binding = shared("shared:team");
    let ws = svc.resolve(&binding).await.unwrap();
    let outsider = svc.exec(&ctx("op-1", "a1"), &binding, exec(TRUE)).await;
    assert_eq!(outsider.unwrap_err().code(), "sandbox_not_attached");
    svc.add_member(&ws.id, &AgentId::new("a1")).await.unwrap();
    let member = svc
        .exec(&ctx("op-2", "a1"), &binding, exec(TRUE))
        .await
        .unwrap();
    assert_eq!(settled(&member), SUCCEEDED);
    let reader = svc.status(&binding, &AgentId::new("a2")).await;
    assert_eq!(reader.unwrap_err().code(), "sandbox_not_attached");
    let forged = Binding {
        owner_scope: "owner:2".into(),
        ..binding.clone()
    };
    let refused = svc.exec(&ctx("op-3", "a1"), &forged, exec(TRUE)).await;
    assert_eq!(refused.unwrap_err().code(), "sandbox_not_attached");
    let folder = Binding {
        kind: WorkspaceKind::HostFolder,
        ..home("a9")
    };
    assert_eq!(
        svc.resolve(&folder).await.unwrap_err().code(),
        "sandbox_unavailable"
    );
    assert_eq!(world.fake.dispatch_calls_total(), 1);
}
