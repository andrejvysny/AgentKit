//! Language-server lifecycle: spawn + `initialize` handshake, crash
//! detection, restart backoff, per-request timeout, cancellation, the
//! document-sync/stale-result request path, and graceful shutdown.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use agentkit_core::{CancellationToken, CoreError};
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Child};

use crate::documents::{DocumentStore, SyncAction};
use crate::error::{
    CANCELLED, REQUEST_TIMEOUT, SERVER_CRASHED, SERVER_ERROR, SERVER_NOT_FOUND, SERVER_UNAVAILABLE,
    STALE_RESULT, err,
};
use crate::jsonrpc::{Connection, NotificationHandler, RequestHook, RpcFailure};
use crate::pool::{LspConfig, ServerCommand};
use crate::uri::path_to_uri;

const STDERR_TAIL_BYTES: usize = 64 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

fn client_capabilities() -> Value {
    json!({
        "textDocument": {
            "definition": {},
            "references": {},
            "hover": {},
            "implementation": {},
            "synchronization": {
                "dynamicRegistration": false,
                "didSave": false,
                "willSave": false
            },
            "publishDiagnostics": { "relatedInformation": false },
            "diagnostic": {}
        },
        "workspace": {
            "symbol": {},
            "configuration": false
        }
    })
}

fn map_failure(failure: RpcFailure, stderr_note: &str) -> CoreError {
    match failure {
        RpcFailure::Disconnected(reason) => err(
            SERVER_CRASHED,
            format!("the language server stopped: {reason}{stderr_note}"),
        ),
        RpcFailure::Rpc {
            code,
            message,
            data,
        } => {
            let detail = data
                .and_then(|d| serde_json::to_string(&d).ok())
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            err(
                SERVER_ERROR,
                format!("language server returned error {code}: {message}{detail}"),
            )
        }
        RpcFailure::Timeout => err(
            REQUEST_TIMEOUT,
            "the language server did not answer before the deadline; the request was cancelled",
        ),
        RpcFailure::Cancelled => err(CANCELLED, "the LSP request was cancelled"),
    }
}

/// One live server generation: child process, JSON-RPC connection,
/// document store and negotiated capabilities.
pub struct ServerProcess {
    conn: Arc<Connection>,
    child: Mutex<Option<Child>>,
    store: Arc<DocumentStore>,
    config: Arc<LspConfig>,
    pub generation: u64,
    pub pid: u32,
    pub root: PathBuf,
    pub language: String,
    pub capabilities: Value,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
}

impl ServerProcess {
    fn stderr_note(&self) -> String {
        let tail = self.stderr_tail.lock().expect("stderr lock").clone();
        if tail.is_empty() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&tail);
        let last = text
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        if last.is_empty() {
            String::new()
        } else {
            format!("; last server stderr: {last}")
        }
    }

    /// True (with reason) once the connection or the child process died.
    pub fn broken_reason(&self) -> Option<String> {
        if let Some(reason) = self.conn.is_broken() {
            return Some(reason);
        }
        let exited = self
            .child
            .lock()
            .expect("child lock")
            .as_mut()
            .map(|child| child.try_wait());
        match exited {
            Some(Ok(Some(status))) => Some(format!("server process exited with {status}")),
            _ => None,
        }
    }

    pub fn is_broken(&self) -> bool {
        self.broken_reason().is_some()
    }

    pub fn store(&self) -> &Arc<DocumentStore> {
        &self.store
    }

    fn has_pull_diagnostics(&self) -> bool {
        self.capabilities
            .get("diagnosticProvider")
            .is_some_and(|provider| !provider.is_null())
    }

    /// Send a request with the pool's per-request timeout; maps transport
    /// failures to the crate's diagnostic codes.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, CoreError> {
        // LSP pins one retry duty on every client: a server whose state
        // moved mid-request (rust-analyzer answers `-32801 content
        // modified` while an index or reparse lands) expects the request
        // to be resent. Without it, a burst of concurrent reads during
        // indexing fails honestly-but-uselessly. Bounded, so a server
        // stuck in content-modified still surfaces as the typed error.
        const CONTENT_MODIFIED: i64 = -32801;
        const RETRIES: usize = 4;
        let mut attempt = 0usize;
        loop {
            let result = self
                .conn
                .request(
                    method,
                    params.clone(),
                    self.config.request_timeout(),
                    cancellation.clone(),
                )
                .await;
            match &result {
                Err(RpcFailure::Rpc { code, .. })
                    if *code == CONTENT_MODIFIED && attempt < RETRIES =>
                {
                    attempt += 1;
                    tokio::select! {
                        _ = cancellation.cancelled() =>
                            return Err(map_failure(RpcFailure::Cancelled, &self.stderr_note())),
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                    }
                }
                _ => return result.map_err(|failure| map_failure(failure, &self.stderr_note())),
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), CoreError> {
        self.conn
            .notify(method, params)
            .await
            .map_err(|failure| map_failure(failure, &self.stderr_note()))
    }

    /// Read the file, open it lazily (or `didChange` it when the on-disk
    /// hash moved on) and return `(uri, docVersion)` to guard requests.
    pub async fn sync_document(&self, path: &Path) -> Result<(String, i64), CoreError> {
        let action = self.store.ensure(path)?;
        match action {
            SyncAction::Opened { uri, text, version } => {
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument":{"uri":uri,"languageId":self.language,
                                            "version":version,"text":text}}),
                )
                .await?;
                Ok((uri, version))
            }
            SyncAction::Changed { uri, text, version } => {
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument":{"uri":uri,"version":version},
                           "contentChanges":[{"text":text}]}),
                )
                .await?;
                Ok((uri, version))
            }
            SyncAction::Unchanged { uri, version } => Ok((uri, version)),
        }
    }

    /// Normalized diagnostics for one document: pull (`textDocument/
    /// diagnostic`) when the server advertises it, else the latest pushed
    /// `publishDiagnostics` set after the configured wait window.
    pub async fn diagnostics_for_uri(&self, uri: &str) -> Result<Vec<Value>, CoreError> {
        if self.has_pull_diagnostics() {
            let result = self
                .request(
                    "textDocument/diagnostic",
                    json!({"textDocument":{"uri":uri}}),
                    &CancellationToken::new(),
                )
                .await?;
            let items = result
                .get("items")
                .or_else(|| result.get("result").and_then(|r| r.get("items")))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            return Ok(crate::documents::normalize_diagnostics(uri, &items));
        }
        let items = self
            .store
            .await_publish(uri, self.config.diagnostics_wait())
            .await
            .unwrap_or_default();
        Ok(crate::documents::normalize_diagnostics(uri, &items))
    }

    /// `shutdown` request, `exit` notification, then SIGKILL fallback
    /// after the grace period. Errors are swallowed — this is teardown.
    pub async fn shutdown(&self) {
        let _ = self
            .conn
            .request(
                "shutdown",
                Value::Null,
                SHUTDOWN_GRACE,
                CancellationToken::new(),
            )
            .await;
        let _ = self.conn.notify("exit", Value::Null).await;
        let child = self.child.lock().expect("child lock").take();
        if let Some(mut child) = child
            && tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
                .await
                .is_err()
        {
            // The group dies too: we spawned it into its own group precisely
            // so forked helpers (rust-analyzer's proc-macro servers,
            // tsserver children) cannot outlive the generation as orphans —
            // a direct-child kill would leave them running.
            kill_group(&child);
            let _ = child.start_kill();
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
        }
    }
}

/// SIGKILL the server's whole process group (pgid == child pid, set at
/// spawn). Best-effort: an already-dead group answers ESRCH, ignored.
#[cfg(unix)]
fn kill_group(child: &tokio::process::Child) {
    if let Some(id) = child.id() {
        // SAFETY: the pid is ours (spawned into its own group); kill(2) of
        // our own group is well-defined, and a dead group just errors.
        unsafe {
            libc::kill(-(id as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_child: &tokio::process::Child) {}
/// Spawn one server generation and run the `initialize` handshake. On any
/// handshake failure (including caller cancellation) the child is killed
/// before the error returns (`kill_on_drop`).
async fn spawn_process(
    root: &Path,
    language: &str,
    command: &ServerCommand,
    config: &Arc<LspConfig>,
    generation: u64,
    cancellation: &CancellationToken,
) -> Result<ServerProcess, CoreError> {
    let mut std_cmd = std::process::Command::new(&command.command);
    // Language servers are third-party binaries fed untrusted workspace
    // content: they run on a MINIMAL environment, never the process
    // environment (which typically holds provider API keys). The config's
    // `env` is the only channel for server-specific variables.
    const SERVER_BASE_ENV: &[&str] = &[
        "PATH",
        "HOME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "LD_LIBRARY_PATH",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        // Windows essentials a server may need to spawn its own children.
        "SYSTEMROOT",
        "COMSPEC",
        "PATHEXT",
        "USERPROFILE",
        "TEMP",
        "TMP",
    ];
    std_cmd
        .args(&command.args)
        .env_clear()
        .envs(
            SERVER_BASE_ENV
                .iter()
                .filter_map(|key| std::env::var_os(key).map(|value| (*key, value))),
        )
        .envs(&command.env)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // Own process group so a runaway server can be taken down wholesale.
        use std::os::unix::process::CommandExt;
        std_cmd.process_group(0);
    }
    let mut builder = tokio::process::Command::from(std_cmd);
    builder.kill_on_drop(true);
    let mut child = builder.spawn().map_err(|e| {
        err(
            SERVER_NOT_FOUND,
            format!(
                "cannot launch {language} server binary {:?}: {e}",
                command.command
            ),
        )
    })?;
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let store = Arc::new(DocumentStore::new());
    let publish_store = Arc::clone(&store);
    let on_notification: NotificationHandler = Arc::new(move |method, params| {
        if method == "textDocument/publishDiagnostics" {
            let uri = params
                .as_ref()
                .and_then(|p| p.get("uri"))
                .and_then(Value::as_str);
            let items = params
                .as_ref()
                .and_then(|p| p.get("diagnostics"))
                .and_then(Value::as_array);
            if let (Some(uri), Some(items)) = (uri, items) {
                publish_store.record_publish(uri, items.clone());
            }
        }
        // window/logMessage, $/progress and unknown notifications: ignored.
    });
    // Server-initiated requests are answered with a null result: this
    // client claims no commands, no work-done tokens, no configuration.
    let on_request: RequestHook = Arc::new(|_method, _params| Box::pin(async { Ok(Value::Null) }));

    let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let tail = Arc::clone(&stderr_tail);
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut tail = tail.lock().expect("stderr lock");
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > STDERR_TAIL_BYTES {
                            let cut = tail.len() - STDERR_TAIL_BYTES;
                            tail.drain(..cut);
                        }
                    }
                }
            }
        });
    }

    let conn = Connection::attach(stdout, stdin, on_notification, on_request);
    let init_params = json!({
        "processId": i64::from(std::process::id()),
        "clientInfo": {"name": "agentkit", "version": env!("CARGO_PKG_VERSION")},
        "rootUri": path_to_uri(root),
        "capabilities": client_capabilities()
    });
    let init = conn
        .request(
            "initialize",
            init_params,
            config.init_timeout(),
            cancellation.clone(),
        )
        .await;
    let capabilities = match init {
        Ok(result) => result
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({})),
        Err(failure) => {
            // Cancellation (or any failure) of initialize aborts startup:
            // the child — and any helper it already forked — dies here
            // before the error escapes (`kill_on_drop` only reaches the
            // direct child, so the group kill is explicit).
            kill_group(&child);
            drop(child);
            return Err(match failure {
                RpcFailure::Cancelled => {
                    err(CANCELLED, "LSP startup was cancelled during initialize")
                }
                other => map_failure(other, ""),
            });
        }
    };
    if let Err(failure) = conn.notify("initialized", json!({})).await {
        kill_group(&child);
        drop(child);
        return Err(map_failure(failure, ""));
    }
    Ok(ServerProcess {
        conn,
        child: Mutex::new(Some(child)),
        store,
        config: Arc::clone(config),
        generation,
        pid,
        root: root.to_path_buf(),
        language: language.to_owned(),
        capabilities,
        stderr_tail,
    })
}

/// A pooled (workspace root, language) slot: lazily spawns generations,
/// restarts a crashed server with a bounded backoff, tracks idle time and
/// runs the stale-guarded document request path.
pub struct ServerHandle {
    pub root: PathBuf,
    pub language: String,
    command: ServerCommand,
    config: Arc<LspConfig>,
    current: Mutex<Option<Arc<ServerProcess>>>,
    init_lock: tokio::sync::Mutex<()>,
    state: Mutex<SlotState>,
    had_process: AtomicBool,
    generation: AtomicU64,
    last_used: Mutex<Instant>,
    /// Requests currently executing against this slot. The idle clock is
    /// frozen while a long `guarded_request` runs (retries can span
    /// several request timeouts, and `diagnostics_all_opened` loops per
    /// document), so eviction MUST consult this — otherwise another
    /// session's pool acquire kills the live server mid-request.
    in_flight: AtomicUsize,
}

struct SlotState {
    restarts: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
}

impl ServerHandle {
    pub fn new(
        root: PathBuf,
        language: String,
        command: ServerCommand,
        config: Arc<LspConfig>,
    ) -> Self {
        Self {
            root,
            language,
            command,
            config,
            current: Mutex::new(None),
            init_lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(SlotState {
                restarts: VecDeque::new(),
                cooldown_until: None,
            }),
            had_process: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            last_used: Mutex::new(Instant::now()),
            in_flight: AtomicUsize::new(0),
        }
    }

    pub fn touch(&self) {
        *self.last_used.lock().expect("last_used lock") = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used.lock().expect("last_used lock").elapsed()
    }

    pub fn is_idle(&self, idle_timeout: Duration) -> bool {
        self.current.lock().expect("current lock").is_some()
            && self.in_flight.load(Ordering::Acquire) == 0
            && self.idle_for() >= idle_timeout
    }

    /// RAII busy marker for one logical request path: pins the slot
    /// against idle eviction for its duration and refreshes the idle
    /// clock on completion (long requests must not age out the server
    /// they are executing on).
    fn busy(&self) -> BusyGuard<'_> {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        BusyGuard { handle: self }
    }
}

struct BusyGuard<'a> {
    handle: &'a ServerHandle,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.handle.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.handle.touch();
    }
}

impl ServerHandle {
    /// Generation of the live process (`None` when broken/absent) — one
    /// half of the stale-result witness.
    pub fn live_generation(&self) -> Option<u64> {
        self.current
            .lock()
            .expect("current lock")
            .as_ref()
            .filter(|process| !process.is_broken())
            .map(|process| process.generation)
    }

    pub fn live_pid(&self) -> Option<u32> {
        self.current
            .lock()
            .expect("current lock")
            .as_ref()
            .filter(|process| !process.is_broken())
            .map(|process| process.pid)
    }

    fn doc_version(&self, uri: &str) -> Option<i64> {
        self.current
            .lock()
            .expect("current lock")
            .as_ref()
            .and_then(|process| process.store().version(uri))
    }

    /// Return a healthy process, spawning/restarting as needed. One init
    /// lock keeps concurrent acquires from double-spawning.
    pub async fn acquire(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<ServerProcess>, CoreError> {
        if cancellation.is_cancelled() {
            return Err(err(CANCELLED, "LSP acquisition was cancelled"));
        }
        self.touch();
        if let Some(process) = self.current.lock().expect("current lock").clone()
            && !process.is_broken()
        {
            return Ok(process);
        }
        let _guard = self.init_lock.lock().await;
        // Re-check: a sibling may have spawned while we waited.
        if let Some(process) = self.current.lock().expect("current lock").clone()
            && !process.is_broken()
        {
            return Ok(process);
        }
        self.current.lock().expect("current lock").take();
        if cancellation.is_cancelled() {
            return Err(err(CANCELLED, "LSP acquisition was cancelled"));
        }
        self.account_restart()?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        match spawn_process(
            &self.root,
            &self.language,
            &self.command,
            &self.config,
            generation,
            cancellation,
        )
        .await
        {
            Ok(process) => {
                self.had_process.store(true, Ordering::SeqCst);
                let process = Arc::new(process);
                *self.current.lock().expect("current lock") = Some(Arc::clone(&process));
                self.touch();
                Ok(process)
            }
            Err(error) => {
                // One spawn attempt per acquire: a crash (child died
                // during initialize) surfaces to the caller and the
                // NEXT acquire restarts — with backoff accounting.
                if error.code == SERVER_CRASHED || error.code == REQUEST_TIMEOUT {
                    self.had_process.store(true, Ordering::SeqCst);
                }
                Err(error)
            }
        }
    }

    /// Restart accounting + backoff refusal; called before every spawn.
    /// Only slots that already ran a process (`had_process`) are subject
    /// to the backoff.
    fn account_restart(&self) -> Result<(), CoreError> {
        if !self.had_process.load(Ordering::SeqCst) {
            return Ok(());
        }
        let now = Instant::now();
        let mut state = self.state.lock().expect("state lock");
        if let Some(until) = state.cooldown_until {
            if now < until {
                let remaining = until.duration_since(now).as_millis();
                return Err(err(
                    SERVER_UNAVAILABLE,
                    format!(
                        "the {} server for {} keeps crashing; unavailable for another {remaining} ms",
                        self.language,
                        self.root.display()
                    ),
                ));
            }
            state.cooldown_until = None;
            state.restarts.clear();
        }
        state
            .restarts
            .retain(|at| now.duration_since(*at) < self.config.restart_window());
        if state.restarts.len() >= self.config.max_restarts {
            state.cooldown_until = Some(now + self.config.restart_window());
            return Err(err(
                SERVER_UNAVAILABLE,
                format!(
                    "the {} server for {} restarted {} times within {} ms; refusing further restarts until the cooldown elapses",
                    self.language,
                    self.root.display(),
                    self.config.max_restarts,
                    self.config.restart_window_ms
                ),
            ));
        }
        state.restarts.push_back(now);
        Ok(())
    }

    /// Stale-guarded request: syncs `file` (when given), captures
    /// `(generation, docVersion)` before dispatch, and — if either moved
    /// while the reply was in flight — discards the reply and retries
    /// exactly once before failing `lsp_stale_result`.
    pub async fn guarded_request(
        self: &Arc<Self>,
        method: &str,
        file: Option<&Path>,
        build_params: &(dyn Fn(&str) -> Value + Sync),
        cancellation: &CancellationToken,
    ) -> Result<Value, CoreError> {
        let _busy = self.busy();
        let mut attempts_left = 1usize;
        loop {
            let process = self.acquire(cancellation).await?;
            let generation = process.generation;
            let document = match file {
                Some(path) => {
                    let (uri, version) = process.sync_document(path).await?;
                    Some((uri, version))
                }
                None => None,
            };
            let params = match &document {
                Some((uri, _)) => build_params(uri),
                None => build_params(""),
            };
            match process.request(method, params, cancellation).await {
                Ok(value) => {
                    let stale = self.live_generation() != Some(generation)
                        || document
                            .as_ref()
                            .is_some_and(|(uri, version)| self.doc_version(uri) != Some(*version));
                    if !stale {
                        return Ok(value);
                    }
                    if attempts_left == 0 {
                        return Err(err(
                            STALE_RESULT,
                            format!(
                                "{method} was answered by a superseded server generation or against a changed document"
                            ),
                        ));
                    }
                    attempts_left -= 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Diagnostics for one confined file path (opens the document first).
    pub async fn diagnostics_for_file(
        self: &Arc<Self>,
        path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Value>, CoreError> {
        let _busy = self.busy();
        let mut attempts_left = 1usize;
        loop {
            let process = self.acquire(cancellation).await?;
            let generation = process.generation;
            let (uri, version) = process.sync_document(path).await?;
            let items = process.diagnostics_for_uri(&uri).await?;
            let stale = self.live_generation() != Some(generation)
                || self.doc_version(&uri) != Some(version);
            if !stale {
                return Ok(items);
            }
            if attempts_left == 0 {
                return Err(err(
                    STALE_RESULT,
                    "diagnostics were served by a superseded server generation",
                ));
            }
            attempts_left -= 1;
        }
    }

    /// Diagnostics for every document opened so far on the live server.
    pub async fn diagnostics_all_opened(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Value>, CoreError> {
        let _busy = self.busy();
        let process = self.acquire(cancellation).await?;
        let mut all = Vec::new();
        for (uri, _) in process.store().opened_uris() {
            self.touch();
            all.extend(process.diagnostics_for_uri(&uri).await?);
        }
        Ok(all)
    }

    /// Graceful shutdown of the live process, if any (idle eviction,
    /// pool close).
    pub async fn close(&self) {
        let process = self.current.lock().expect("current lock").take();
        if let Some(process) = process {
            process.shutdown().await;
        }
    }
}
