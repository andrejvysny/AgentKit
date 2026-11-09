//! M2 routing tests: with `PackEnv::vm` set, every sandbox-bound tool
//! call goes through `SandboxWorkspaceIo` and NEVER touches host I/O
//! (S06). The fake io below is the judge: it records every grant and
//! asserts the digest is always the zero-form scaffold (S07 — the pack
//! passes `grant_scaffold`'s output through unchanged), serves scripted
//! guest state, and fails the test if the pack ever looked at the host.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use agentkit_core::{
    CoreError, DispatchFence, DispatchGuard, Tool, ToolContext, ToolLimits, ToolResult,
};

use agentkit_sandbox_contracts::api::{
    CapabilityManifest, DeletionReceipt, EntryKind, ExecArgv, ExecEvent, ExecEventKind,
    ExecRequest, ExecutionHandle, GitCommand, GitInspectQuery, GitInspectReply, ListEntry,
    ListOptions, ListReply, ReadReply, SandboxManager, SandboxRef, SandboxStateInfo,
    SandboxWorkspaceIo, SearchQuery, SearchReply, WriteReceipt,
};
use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};
use agentkit_sandbox_contracts::grant::{AuthContext, WorkspaceGrant};
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, ExecutionId, InstanceGeneration, PolicyDigest, PolicyGeneration, PrincipalId,
    RequestDigest, SandboxId, SessionId, TenantId,
};
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_contracts::policy::SandboxCapability;
use agentkit_sandbox_contracts::session::VmSandboxSession;
use agentkit_sandbox_contracts::state::CommandState;

use agentkit_toolpacks_coding::context::{EnvInit, PackEnv};
use agentkit_toolpacks_coding::coordination::default_in_process_coordinator;
use agentkit_toolpacks_coding::limits::CodingLimits;
use agentkit_toolpacks_coding::types::SandboxMode;
use agentkit_toolpacks_coding::{exec_tools, fs_tools, git_tool, search_tools};

fn zero_form() -> String {
    // The contract's zero form: what `grant_scaffold` mints and what the
    // bridge must replace before dispatch (S07). Compared via the single
    // authority itself, not a hard-coded hex.
    RequestDigest::of_canonical_json(b"").as_str().to_owned()
}

#[derive(Clone, Debug)]
struct Call {
    op: &'static str,
    path: Option<String>,
    grant_digest_zero: bool,
    capability: SandboxCapability,
}

/// Guest state + scripted replies + the complete record of what the pack
/// actually asked for.
#[derive(Default)]
struct FakeIo {
    calls: Mutex<Vec<Call>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    dirs: Mutex<HashMap<String, Vec<ListEntry>>>,
    search_reply: Mutex<Option<SearchReply>>,
    git_reply: Mutex<Option<GitInspectReply>>,
    last_git_query: Mutex<Option<GitInspectQuery>>,
    exec_results: Mutex<VecDeque<Result<ExecutionHandle, SandboxError>>>,
    /// Polled until `exec_cancel` is recorded; afterwards polls drain this.
    initial_events: Mutex<VecDeque<Vec<ExecEvent>>>,
    post_cancel_events: Mutex<VecDeque<Vec<ExecEvent>>>,
    cancels: Mutex<Vec<ExecutionId>>,
    poison: Mutex<Option<String>>, // if set, any op returns this error instead
}

use std::sync::Mutex;

impl FakeIo {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn record(&self, op: &'static str, grant: &WorkspaceGrant, path: Option<String>) {
        let zero = grant.intended_request_digest.as_str() == zero_form();
        // S07: the pack must pass `grant_scaffold` output through with the
        // zero-form digest; only the bridge fills real digests.
        assert!(
            zero,
            "pack sent a non-zero intended_request_digest ({}); the pack \
             must never compute digests",
            grant.intended_request_digest
        );
        self.calls.lock().unwrap().push(Call {
            op,
            path,
            grant_digest_zero: true,
            capability: grant.capability,
        });
    }

    fn put(&self, path: &str, content: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), content.to_vec());
    }

    fn dir(&self, path: &str, entries: Vec<ListEntry>) {
        self.dirs.lock().unwrap().insert(path.to_owned(), entries);
    }

    fn ops(&self) -> Vec<(&'static str, Option<String>)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| (call.op, call.path.clone()))
            .collect()
    }

    fn fail(&self, code: &str) {
        *self.poison.lock().unwrap() = Some(code.to_owned());
    }
}

#[async_trait]
impl SandboxWorkspaceIo for FakeIo {
    async fn read(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        from: Option<u64>,
        len: Option<u64>,
    ) -> Result<ReadReply, SandboxError> {
        self.record("read", grant, Some(path.as_str().to_owned()));
        if let Some(code) = self.poison.lock().unwrap().as_deref() {
            return Err(poison(code));
        }
        let files = self.files.lock().unwrap();
        let Some(bytes) = files.get(path.as_str()) else {
            return Err(SandboxError::new(
                SandboxErrorCode::NotFound,
                format!("no such guest file: {}", path),
            ));
        };
        let total = bytes.len() as u64;
        let start = from.unwrap_or(0).min(total);
        let end = len.map_or(total, |len| start.saturating_add(len).min(total));
        let slice = bytes[start as usize..end as usize].to_vec();
        Ok(ReadReply {
            data_b64: base64::engine::general_purpose::STANDARD.encode(&slice),
            sha256: ArtifactDigest::of_bytes(&slice),
            total_size: total,
            from: start,
            next_from: if end < total { Some(end) } else { None },
            truncated: end < total,
            modified_ms: None,
        })
    }

    async fn write(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        body: Vec<u8>,
        expected_sha256: Option<ArtifactDigest>,
    ) -> Result<WriteReceipt, SandboxError> {
        self.record("write", grant, Some(path.as_str().to_owned()));
        if let Some(code) = self.poison.lock().unwrap().as_deref() {
            return Err(poison(code));
        }
        let mut files = self.files.lock().unwrap();
        let current = files.get(path.as_str()).cloned();
        if let Some(expected) = &expected_sha256 {
            match &current {
                None => {
                    return Err(SandboxError::new(
                        SandboxErrorCode::NotFound,
                        "CAS: guest file vanished",
                    ));
                }
                Some(bytes) if &ArtifactDigest::of_bytes(bytes) != expected => {
                    return Err(SandboxError::new(
                        SandboxErrorCode::ArtifactInvalid,
                        "CAS: guest digest moved under the call",
                    ));
                }
                _ => {}
            }
        }
        let created = current.is_none();
        let sha256 = ArtifactDigest::of_bytes(&body);
        let bytes_written = body.len() as u64;
        files.insert(path.as_str().to_owned(), body);
        Ok(WriteReceipt {
            path: path.clone(),
            sha256,
            bytes_written,
            created,
        })
    }

    async fn list(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        _options: ListOptions,
    ) -> Result<ListReply, SandboxError> {
        self.record("list", grant, Some(path.as_str().to_owned()));
        if let Some(code) = self.poison.lock().unwrap().as_deref() {
            return Err(poison(code));
        }
        let dirs = self.dirs.lock().unwrap();
        match dirs.get(path.as_str()) {
            Some(entries) => Ok(ListReply {
                entries: entries.clone(),
                truncated: false,
            }),
            None => Err(SandboxError::new(
                SandboxErrorCode::NotFound,
                format!("no such guest directory: {}", path),
            )),
        }
    }

    async fn search(
        &self,
        grant: &WorkspaceGrant,
        query: SearchQuery,
    ) -> Result<SearchReply, SandboxError> {
        self.record("search", grant, Some(query.root.as_str().to_owned()));
        if let Some(code) = self.poison.lock().unwrap().as_deref() {
            return Err(poison(code));
        }
        self.search_reply
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                SandboxError::new(SandboxErrorCode::Unavailable, "no scripted search reply")
            })
    }

    async fn git_inspect(
        &self,
        grant: &WorkspaceGrant,
        query: GitInspectQuery,
    ) -> Result<GitInspectReply, SandboxError> {
        self.record("git_inspect", grant, Some(query.repo.as_str().to_owned()));
        if let Some(code) = self.poison.lock().unwrap().as_deref() {
            return Err(poison(code));
        }
        *self.last_git_query.lock().unwrap() = Some(query);
        self.git_reply.lock().unwrap().clone().ok_or_else(|| {
            SandboxError::new(SandboxErrorCode::Unavailable, "no scripted git reply")
        })
    }

    async fn exec(
        &self,
        grant: &WorkspaceGrant,
        request: ExecRequest,
    ) -> Result<ExecutionHandle, SandboxError> {
        self.record("exec", grant, Some(request.cwd.as_str().to_owned()));
        assert_eq!(
            request.request_digest.as_str(),
            zero_form(),
            "the pack must send the zero-form request digest; the bridge \
             fills it (S07)"
        );
        assert!(
            matches!(request.argv, ExecArgv::Shell { .. }),
            "coding pack commands travel in Shell form"
        );
        if let Some(code) = self.poison.lock().unwrap().as_deref() {
            return Err(poison(code));
        }
        self.exec_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Err(SandboxError::new(
                    SandboxErrorCode::Unavailable,
                    "no scripted exec result",
                ))
            })
    }

    async fn exec_events(
        &self,
        grant: &WorkspaceGrant,
        execution_id: &ExecutionId,
        after_sequence: u64,
    ) -> Result<Vec<ExecEvent>, SandboxError> {
        self.record("exec_events", grant, None);
        if let Some(code) = self.poison.lock().unwrap().as_deref() {
            return Err(poison(code));
        }
        let cancelled = !self.cancels.lock().unwrap().is_empty();
        let queues = if cancelled {
            &self.post_cancel_events
        } else {
            &self.initial_events
        };
        let batch: Vec<ExecEvent> = queues.lock().unwrap().pop_front().unwrap_or_default();
        Ok(batch
            .into_iter()
            .filter(|event| event.execution_id == *execution_id && event.sequence > after_sequence)
            .collect())
    }

    async fn exec_cancel(
        &self,
        grant: &WorkspaceGrant,
        execution_id: &ExecutionId,
    ) -> Result<agentkit_sandbox_contracts::api::CommandStateOut, SandboxError> {
        self.record("exec_cancel", grant, None);
        if let Some(code) = self.poison.lock().unwrap().as_deref() {
            return Err(poison(code));
        }
        self.cancels.lock().unwrap().push(execution_id.clone());
        Ok(agentkit_sandbox_contracts::api::CommandStateOut {
            execution_id: execution_id.clone(),
            command_state: CommandState::Dispatched,
            uncertain: false,
        })
    }
}

fn poison(code: &str) -> SandboxError {
    let code = match code {
        "not_found" => SandboxErrorCode::NotFound,
        "policy_denied" => SandboxErrorCode::PolicyDenied,
        "unavailable" => SandboxErrorCode::Unavailable,
        "resource_exhausted" => SandboxErrorCode::ResourceExhausted,
        "quarantined" => SandboxErrorCode::Quarantined,
        "protocol" => SandboxErrorCode::ProtocolIncompatible,
        _ => SandboxErrorCode::ArtifactInvalid,
    };
    SandboxError::new(code, format!("poisoned fake: {code:?}"))
}

/// The manager seam is never exercised by tool routing; every call would
/// be a routing bug, so it screams.
struct ScreamingManager;

#[async_trait]
impl SandboxManager for ScreamingManager {
    async fn capabilities(&self, _ctx: &AuthContext) -> Result<CapabilityManifest, SandboxError> {
        Err(SandboxError::new(
            SandboxErrorCode::ProtocolIncompatible,
            "tools must not call the manager",
        ))
    }
    async fn create(
        &self,
        _ctx: &AuthContext,
        _spec: &agentkit_sandbox_contracts::spec::SandboxSpec,
    ) -> Result<SandboxRef, SandboxError> {
        unreachable!("tools must not call the manager")
    }
    async fn start(
        &self,
        _ctx: &AuthContext,
        _id: &SandboxId,
    ) -> Result<SandboxStateInfo, SandboxError> {
        unreachable!("tools must not call the manager")
    }
    async fn inspect(
        &self,
        _ctx: &AuthContext,
        _id: &SandboxId,
    ) -> Result<SandboxStateInfo, SandboxError> {
        unreachable!("tools must not call the manager")
    }
    async fn stop(
        &self,
        _ctx: &AuthContext,
        _id: &SandboxId,
    ) -> Result<SandboxStateInfo, SandboxError> {
        unreachable!("tools must not call the manager")
    }
    async fn destroy(
        &self,
        _ctx: &AuthContext,
        _id: &SandboxId,
    ) -> Result<DeletionReceipt, SandboxError> {
        unreachable!("tools must not call the manager")
    }
}

fn session(io: Arc<FakeIo>) -> Arc<VmSandboxSession> {
    Arc::new(VmSandboxSession {
        tenant_id: TenantId::new("tenant-a").unwrap(),
        principal_id: PrincipalId::new("principal-a").unwrap(),
        session_id: SessionId::new("session-a").unwrap(),
        agent_id: None,
        sandbox_id: SandboxId::new("sandbox-a").unwrap(),
        instance_generation: InstanceGeneration(1),
        policy_generation: PolicyGeneration(1),
        policy_digest: PolicyDigest::of_bytes(b"policy"),
        capabilities: vec![
            SandboxCapability::SandboxRead,
            SandboxCapability::SandboxWrite,
            SandboxCapability::ProcessExec,
        ],
        permitted_paths: vec![GuestPath::parse("/workspace").unwrap()],
        default_cwd: GuestPath::parse("/workspace").unwrap(),
        grant_ttl_ms: 60_000,
        manager: Arc::new(ScreamingManager),
        io,
    })
}

/// A `PackEnv` whose workspace root does not exist on the host: any code
/// path that touched the host filesystem would fail with an io error
/// instead of the scripted guest result — the poisoned-host proof.
fn env_with(io: Arc<FakeIo>) -> Arc<PackEnv> {
    // Unique per pack: the in-process coordinator is process-global and
    // the lease key is the root string — concurrent tests must not share
    // an exclusive workspace lease. The path still does not exist on the
    // host, which is the whole point.
    let workspace_root = format!("/nonexistent/vm-routing-host-root/{}", uuid::Uuid::new_v4());
    Arc::new(PackEnv::new(EnvInit {
        workspace_root,
        limits: CodingLimits::default(),
        allow_sensitive: false,
        clock: Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
        }),
        sandbox: SandboxMode::Off,
        network: false,
        allow_git_writes: false,
        test_commands: vec!["cargo test".to_owned()],
        platform: "linux".to_owned(),
        sandbox_exec_path: "/nonexistent/sandbox-exec".to_owned(),
        coordinator: default_in_process_coordinator(),
        coordination_wait_ms: 1_000,
        environment: None,
        vm: Some(session(io)),
    }))
}

fn entry(name: &str, kind: EntryKind, size: u64) -> ListEntry {
    ListEntry {
        name: name.to_owned(),
        kind,
        size,
    }
}

fn events(exec: &ExecutionId, kinds: &[(ExecEventKind, u64)]) -> Vec<ExecEvent> {
    kinds
        .iter()
        .map(|(kind, seq)| ExecEvent {
            execution_id: exec.clone(),
            sequence: *seq,
            kind: kind.clone(),
        })
        .collect()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[derive(Debug)]
struct CountingGuard {
    allow: bool,
    checks: Arc<AtomicUsize>,
}

#[async_trait]
impl DispatchGuard for CountingGuard {
    async fn check(&self) -> Result<(), CoreError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if self.allow {
            Ok(())
        } else {
            Err(CoreError::new("permission_denied", "user denied"))
        }
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "task".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}
fn ctx_with(
    chat: &str,
    token: CancellationToken,
    guard: Option<Arc<dyn DispatchGuard>>,
) -> ToolContext {
    ToolContext {
        suspension: None,
        run_id: "run-vm".to_owned(),
        tool_call_id: Some("call-vm-1".to_owned()),
        attempt_id: Some("attempt-1".to_owned()),
        chat_id: Some(chat.to_owned()),
        scope_id: None,
        user_id: None,
        bindings: vec![],
        limits: ToolLimits::default(),
        cancellation: token,
        dispatch_guard: guard,
        metadata: serde_json::Map::new(),
    }
}

async fn call_chat(
    _env: &Arc<PackEnv>,
    tool: &Arc<dyn Tool>,
    chat: &str,
    input: Value,
) -> ToolResult {
    tool.execute(ctx_with(chat, CancellationToken::new(), None), input)
        .await
        .expect("tool bodies never throw")
}

fn model_text(result: &ToolResult) -> String {
    result
        .model_data
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_default()
}

#[tokio::test]
async fn read_file_serves_guest_bytes_and_never_touches_the_host() {
    let io = FakeIo::new();
    io.put("/workspace/a.txt", b"guest contents only\nsecond line\n");
    let env = env_with(Arc::clone(&io));
    let result = call_chat(
        &env,
        &fs_tools::read_file_tool(&env),
        "read-basic",
        json!({"path": "a.txt"}),
    )
    .await;
    assert!(result.ok, "{:?}", model_text(&result));
    let text = model_text(&result);
    assert!(text.contains("guest contents only"), "{text}");
    // The host root does not exist; had any host path run, the call would
    // be an io error, not the guest's bytes. And the record shows exactly
    // one guest read, Read-capped, zero-form grant.
    assert!(
        matches!(io.ops().as_slice(), [("read", Some(p))] if p == "/workspace/a.txt"),
        "{:?}",
        io.ops()
    );
    let calls = io.calls.lock().unwrap();
    assert_eq!(calls[0].capability, SandboxCapability::SandboxRead);
    assert!(calls[0].grant_digest_zero);
}

#[tokio::test]
async fn read_poisoned_io_error_surfaces_the_sandbox_code() {
    let io = FakeIo::new();
    io.fail("resource_exhausted");
    let env = env_with(Arc::clone(&io));
    let result = call_chat(
        &env,
        &fs_tools::read_file_tool(&env),
        "read-poison",
        json!({"path": "x.txt"}),
    )
    .await;
    assert!(!result.ok);
    let text = model_text(&result);
    // The stable machine-readable code rides in the tool error; there is
    // no host retry behind it (S06).
    assert!(text.contains("sandbox_resource_exhausted"), "{text}");
}

#[tokio::test]
async fn write_create_overwrite_cas_and_read_before_write_gate() {
    let io = FakeIo::new();
    let env = env_with(Arc::clone(&io));
    // Create without a prior read is allowed.
    let created = call_chat(
        &env,
        &fs_tools::write_file_tool(&env),
        "write-create",
        json!({"path": "new.txt", "content": "hello"}),
    )
    .await;
    assert!(created.ok, "{:?}", model_text(&created));
    assert_eq!(
        io.files
            .lock()
            .unwrap()
            .get("/workspace/new.txt")
            .map(Vec::as_slice),
        Some(&b"hello"[..])
    );

    // Overwriting a file this chat never read: refused, and the guest
    // write NEVER happened (no fallback, no blind overwrite).
    io.put("/workspace/other.txt", b"untouched\n");
    let refused = call_chat(
        &env,
        &fs_tools::write_file_tool(&env),
        "write-blind",
        json!({"path": "other.txt", "content": "clobber"}),
    )
    .await;
    assert!(!refused.ok);
    assert!(
        model_text(&refused).contains("file_not_read"),
        "{}",
        model_text(&refused)
    );
    assert_eq!(
        io.files
            .lock()
            .unwrap()
            .get("/workspace/other.txt")
            .map(Vec::as_slice),
        Some(&b"untouched\n"[..])
    );

    // Read first, then the guest's CAS refuses a stale overwrite.
    let read = call_chat(
        &env,
        &fs_tools::read_file_tool(&env),
        "write-cas",
        json!({"path": "other.txt"}),
    )
    .await;
    assert!(read.ok);
    io.put("/workspace/other.txt", b"moved under us\n"); // racing writer
    let stale = call_chat(
        &env,
        &fs_tools::write_file_tool(&env),
        "write-cas",
        json!({"path": "other.txt", "content": "late"}),
    )
    .await;
    assert!(!stale.ok);
    assert!(
        model_text(&stale).contains("file_changed"),
        "{}",
        model_text(&stale)
    );
    assert_eq!(
        io.files
            .lock()
            .unwrap()
            .get("/workspace/other.txt")
            .map(Vec::as_slice),
        Some(&b"moved under us\n"[..]),
        "the CAS loser must not have written"
    );
}

#[tokio::test]
async fn list_dir_renders_guest_entries_through_the_host_pipeline() {
    let io = FakeIo::new();
    io.dir(
        "/workspace",
        vec![
            entry("b.txt", EntryKind::File, 3),
            entry("a.txt", EntryKind::File, 2),
            entry("sub", EntryKind::Dir, 0),
        ],
    );
    let env = env_with(Arc::clone(&io));
    let result = call_chat(
        &env,
        &fs_tools::list_dir_tool(&env),
        "list-basic",
        json!({}),
    )
    .await;
    assert!(result.ok, "{:?}", model_text(&result));
    let text = model_text(&result);
    assert!(
        text.contains("a.txt") && text.contains("b.txt") && text.contains("sub/"),
        "{text}"
    );
    // Sorted like the host listing (UTF-16 order on names).
    let a = text.find("a.txt").unwrap();
    let b = text.find("b.txt").unwrap();
    assert!(a < b, "{text}");
}

#[tokio::test]
async fn code_search_uses_the_guest_engine_and_host_formatting() {
    let io = FakeIo::new();
    io.dir("/workspace", vec![entry("src", EntryKind::Dir, 0)]);
    *io.search_reply.lock().unwrap() = Some(SearchReply {
        matches: vec![agentkit_sandbox_contracts::api::SearchMatch {
            path: "src/a.rs".to_owned(),
            line: 3,
            text: "let needle = 1;".to_owned(),
            context_before: vec![],
            context_after: vec![],
        }],
        files_scanned: 1,
        truncated: false,
        timed_out: false,
    });
    let env = env_with(Arc::clone(&io));
    let result = call_chat(
        &env,
        &search_tools::code_search_tool(&env),
        "search-basic",
        json!({"pattern": "needle"}),
    )
    .await;
    assert!(result.ok, "{:?}", model_text(&result));
    let text = model_text(&result);
    assert!(text.contains("src/a.rs:3:let needle = 1;"), "{text}");
    assert_eq!(result.summary.as_deref(), Some("1 match(es) for needle"));
    let ops = io.ops();
    assert!(ops.iter().any(|(op, _)| *op == "search"), "{ops:?}");
}

#[tokio::test]
async fn git_inspect_routes_reads_and_refuses_everything_else() {
    let io = FakeIo::new();
    let reply = GitInspectReply {
        text: "On branch main\nnothing to commit".to_owned(),
        sha256: ArtifactDigest::of_bytes(b"On branch main\nnothing to commit"),
        truncated: false,
        exit_code: 0,
    };
    *io.git_reply.lock().unwrap() = Some(reply);
    let env = env_with(Arc::clone(&io));

    let status = call_chat(
        &env,
        &git_tool::git_inspect_tool(&env),
        "git-status",
        json!({"command": "status"}),
    )
    .await;
    assert!(status.ok, "{:?}", model_text(&status));
    assert!(model_text(&status).contains("On branch main"));
    assert_eq!(status.summary.as_deref(), Some("git status"));
    let query = io.last_git_query.lock().unwrap().clone().unwrap();
    assert_eq!(query.command, GitCommand::Status);
    assert_eq!(query.repo.as_str(), "/workspace");
    assert_eq!(query.max_count, 20);
    assert!(query.path_filters.is_empty());

    // A write command under a vm target is a policy denial — no host git.
    let commit = call_chat(
        &env,
        &git_tool::git_inspect_tool(&env),
        "git-commit",
        json!({"command": "commit"}),
    )
    .await;
    assert!(!commit.ok);
    assert!(
        model_text(&commit).contains("sandbox_policy_denied"),
        "{}",
        model_text(&commit)
    );

    // The typed inspect request has no staged axis: refused, not faked.
    let staged = call_chat(
        &env,
        &git_tool::git_inspect_tool(&env),
        "git-staged",
        json!({"command": "diff", "args": {"staged": true}}),
    )
    .await;
    assert!(!staged.ok);
    assert!(
        model_text(&staged).contains("sandbox_policy_denied"),
        "{}",
        model_text(&staged)
    );
}

fn handle_for(id: &str) -> ExecutionHandle {
    ExecutionHandle {
        execution_id: ExecutionId::new(id).unwrap(),
        state: CommandState::Dispatched,
        replayed: false,
    }
}

#[tokio::test]
async fn exec_happy_path_streams_guest_output_to_terminal_state() {
    let io = FakeIo::new();
    let exec = ExecutionId::new("7-exec-happy").unwrap();
    io.exec_results
        .lock()
        .unwrap()
        .push_back(Ok(handle_for("7-exec-happy")));
    io.initial_events.lock().unwrap().push_back(events(
        &exec,
        &[
            (ExecEventKind::Accepted { at_ms: 1 }, 1),
            (
                ExecEventKind::Spawned {
                    at_ms: 1,
                    guest_pid: 42,
                },
                2,
            ),
            (
                ExecEventKind::StdoutChunk {
                    bytes_b64: b64(b"hello "),
                },
                3,
            ),
            (
                ExecEventKind::StderrChunk {
                    bytes_b64: b64(b"world"),
                },
                4,
            ),
            (
                ExecEventKind::Exited {
                    exit_code: Some(0),
                    signal: None,
                    timed_out: false,
                    at_ms: 2,
                },
                5,
            ),
        ],
    ));
    let env = env_with(Arc::clone(&io));
    let result = call_chat(
        &env,
        &exec_tools::run_command_tool(&env),
        "exec-happy",
        json!({"command": "echo hello", "timeoutSec": 30}),
    )
    .await;
    assert!(result.ok, "{:?}", model_text(&result));
    let text = model_text(&result);
    assert!(
        text.contains("hello world") && text.contains("[exit code 0]"),
        "{text}"
    );
    assert_eq!(result.summary.as_deref(), Some("exit code 0"));
    assert!(io.cancels.lock().unwrap().is_empty());
    let ops: Vec<&str> = io.ops().into_iter().map(|(op, _)| op).collect();
    assert!(
        ops.contains(&"exec") && ops.contains(&"exec_events"),
        "{ops:?}"
    );
    assert!(!ops.contains(&"exec_cancel"));
}

#[tokio::test]
async fn exec_admission_error_surfaces_the_sandbox_code_without_fallback() {
    let io = FakeIo::new();
    io.exec_results
        .lock()
        .unwrap()
        .push_back(Err(SandboxError::new(
            SandboxErrorCode::PolicyDenied,
            "argv refused by the guest",
        )));
    let env = env_with(Arc::clone(&io));
    let result = call_chat(
        &env,
        &exec_tools::run_command_tool(&env),
        "exec-denied",
        json!({"command": "touch /workspace/pwned"}),
    )
    .await;
    assert!(!result.ok);
    let text = model_text(&result);
    assert!(text.contains("sandbox_policy_denied"), "{text}");
    // No event polling ever started, no cancel, no host execution.
    let ops: Vec<&str> = io.ops().into_iter().map(|(op, _)| op).collect();
    assert!(!ops.contains(&"exec_events"), "{ops:?}");
    assert!(io.cancels.lock().unwrap().is_empty());
}

#[tokio::test]
async fn exec_wall_deadline_requests_cancel_and_reaches_terminal() {
    let io = FakeIo::new();
    let exec = ExecutionId::new("7-exec-timeout").unwrap();
    io.exec_results
        .lock()
        .unwrap()
        .push_back(Ok(handle_for("7-exec-timeout")));
    // The guest reports nothing until the cancel: first polls come back
    // empty, then the terminal arrives after the cancel was requested.
    io.initial_events.lock().unwrap().push_back(Vec::new());
    io.post_cancel_events.lock().unwrap().push_back(events(
        &exec,
        &[(
            ExecEventKind::Exited {
                exit_code: None,
                signal: Some(9),
                timed_out: false,
                at_ms: 9,
            },
            1,
        )],
    ));
    let env = env_with(Arc::clone(&io));
    let result = call_chat(
        &env,
        &exec_tools::run_command_tool(&env),
        "exec-timeout",
        json!({"command": "sleep 100", "timeoutSec": 1}),
    )
    .await;
    assert!(!result.ok);
    let text = model_text(&result);
    assert!(text.contains("timed out"), "{text}");
    // The cancel was REQUESTED (S14) — the terminal state was then reached.
    assert_eq!(io.cancels.lock().unwrap().len(), 1);
    let details = result.data.clone().unwrap();
    assert_eq!(details["timedOut"], json!(true));
    assert_eq!(details["teardown"]["cancelRequested"], json!(true));
}

#[tokio::test]
async fn exec_uncertain_after_cancel_keeps_the_error_code_and_never_retries() {
    let io = FakeIo::new();
    let exec = ExecutionId::new("7-exec-uncertain").unwrap();
    io.exec_results
        .lock()
        .unwrap()
        .push_back(Ok(handle_for("7-exec-uncertain")));
    // Never anything terminal, ever.
    let env = env_with(Arc::clone(&io));
    // 1s wall + 5s dispatch grace + 10s cancel grace would be 16 real
    // seconds; drive the same path through the guest's own Uncertain
    // verdict instead, which is the identical settlement branch.
    io.initial_events.lock().unwrap().push_back(events(
        &exec,
        &[(
            ExecEventKind::Uncertain {
                reason: "guest lost the process".to_owned(),
            },
            1,
        )],
    ));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        call_chat(
            &env,
            &exec_tools::run_command_tool(&env),
            "exec-uncertain",
            json!({"command": "mystery", "timeoutSec": 30}),
        ),
    )
    .await
    .expect("settles via the Uncertain event");
    assert!(!result.ok);
    let text = model_text(&result);
    assert!(text.contains("sandbox_execution_uncertain"), "{text}");
    assert!(text.contains("guest lost the process"), "{text}");
}

#[tokio::test]
async fn guard_denial_precedes_every_io_call() {
    let io = FakeIo::new();
    let env = env_with(Arc::clone(&io));
    let checks = Arc::new(AtomicUsize::new(0));
    let guard = Arc::new(CountingGuard {
        allow: false,
        checks: Arc::clone(&checks),
    });
    let result = exec_tools::run_command_tool(&env)
        .execute(
            ctx_with("guard-deny", CancellationToken::new(), Some(guard)),
            json!({"command": "ls"}),
        )
        .await
        .unwrap();
    assert!(!result.ok);
    assert!(
        checks.load(Ordering::SeqCst) >= 1,
        "the decision gate must run first"
    );
    // Nothing reached the guest: the guard gates the decision, and the
    // denial precedes ANY session call (grant, exec, or events).
    assert!(io.ops().is_empty(), "{:?}", io.ops());
    let details = result.data.clone().unwrap();
    assert_eq!(details["errorCode"], json!("command_not_started"));
    assert!(
        details["errorMessage"]
            .as_str()
            .unwrap()
            .contains("permission_denied")
    );
}

#[tokio::test]
async fn guard_approval_still_routes_with_zero_form_scaffolds() {
    let io = FakeIo::new();
    let exec = ExecutionId::new("7-exec-guarded").unwrap();
    io.exec_results
        .lock()
        .unwrap()
        .push_back(Ok(handle_for("7-exec-guarded")));
    io.initial_events.lock().unwrap().push_back(events(
        &exec,
        &[(
            ExecEventKind::Exited {
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                at_ms: 1,
            },
            1,
        )],
    ));
    let env = env_with(Arc::clone(&io));
    let checks = Arc::new(AtomicUsize::new(0));
    let guard = Arc::new(CountingGuard {
        allow: true,
        checks: Arc::clone(&checks),
    });
    let result = exec_tools::run_command_tool(&env)
        .execute(
            ctx_with("guard-allow", CancellationToken::new(), Some(guard)),
            json!({"command": "true"}),
        )
        .await
        .unwrap();
    assert!(result.ok, "{:?}", model_text(&result));
    // AND-gate: host guard authorized (twice: pre-acquisition and
    // immediately pre-dispatch) and the guest executed; every grant that
    // reached the io layer carried the zero-form scaffold digest (asserted
    // inside the fake), which is exactly what the bridge must fill.
    assert!(
        checks.load(Ordering::SeqCst) >= 2,
        "guard must re-check at the dispatch boundary"
    );
    let calls = io.calls.lock().unwrap();
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|call| call.grant_digest_zero));
    assert!(
        calls
            .iter()
            .any(|call| call.op == "exec" && call.capability == SandboxCapability::ProcessExec)
    );
}

#[tokio::test]
async fn writer_lease_injection_point_defaults_to_none_on_grants() {
    // v1 contract: mutating routed calls carry NO writer lease token; the
    // injection point exists on PackEnv for the bridge to set later.
    let io = FakeIo::new();
    let env = env_with(Arc::clone(&io));
    assert!(env.vm_writer_lease.is_none());
    let result = call_chat(
        &env,
        &fs_tools::write_file_tool(&env),
        "lease-none",
        json!({"path": "w.txt", "content": "x"}),
    )
    .await;
    assert!(result.ok, "{:?}", model_text(&result));
}
// TESTS-CONTINUE
