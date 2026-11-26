//! A fake launcher on a unix socket: speaks the launcher frames, keeps
//! "created through" and "stopped through" per workspace in memory,
//! creates the per-machine socket directory
//! (`<socket_dir>/<machine_dir_name(ws, gen)>`), gives each container a
//! fresh channel token and starts it through a [`Starter`] (an in-process
//! supervisor, a scripted guest, or nothing). Volumes live under an
//! optional data directory.
//!
//! `ensure` follows the real launcher's rules (`sandbox-launcher`'s
//! `service/ensure.rs`): a generation gets at most one container, ever.
//! A container exists from `ensure` until a `stop` removes it, also after
//! its task ended (an exited container). So:
//! - at or below `stopped_through`, below `created_through` or below the
//!   existing container's generation: `stale_generation` (current: the
//!   highest of the three);
//! - a container of another (lower) generation exists: `busy`;
//! - no container and the generation was created already: `busy`;
//! - the generation's container exists but ended: `busy`;
//! - its container runs: `ensured` with it and its token; else it is
//!   created with a new token.
//!
//! `stop` of a generation removes the workspace's container when its
//! generation is that one or lower, as the real launcher does.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentkit_sandbox_proto::launcher::{
    LauncherError, LauncherErrorCode, Reply, Request, RequestEnvelope,
};
use agentkit_sandbox_proto::{
    Envelope, Frame, FrameDecoder, encode_control, machine_dir_name, parse_control,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

/// What the launcher starts for one machine.
pub struct Start {
    pub workspace_id: String,
    pub generation: u64,
    /// `<socket_dir>/<machine_dir_name(ws, gen)>`.
    pub dir: PathBuf,
    /// The container's channel token (its `AGENTKIT_CHANNEL_TOKEN`).
    pub channel_token: String,
}

/// The workspace's one container.
struct Container {
    generation: u64,
    /// Ends when the container would.
    task: JoinHandle<()>,
    token: String,
}

/// Starts a "container": a task that ends when the container would.
pub type Starter = Arc<dyn Fn(Start) -> JoinHandle<()> + Send + Sync>;

struct State {
    digest: String,
    starter: Starter,
    data_dir: Option<PathBuf>,
    socket_dir: PathBuf,
    /// Owns `socket_dir`, as long as the launcher serves.
    _socket_dir: tempfile::TempDir,
    /// The workspace's container (one at most). Removed only by `stop`.
    running: HashMap<String, Container>,
    stopped_through: HashMap<String, u64>,
    created_through: HashMap<String, u64>,
    calls: Vec<String>,
    fail_ensure: Option<LauncherError>,
    fail_usage: Option<LauncherError>,
    unprove_stop: bool,
    tokens_made: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running fake launcher.
#[derive(Clone)]
pub struct FakeLauncher {
    pub socket: PathBuf,
    pub socket_dir: PathBuf,
    state: Arc<Mutex<State>>,
}

impl FakeLauncher {
    /// Serve on `<root>/launcher.sock`, with the machine sockets under a
    /// short directory in `/tmp` (`/tmp/akXXXX`): the adapter refuses a
    /// socket directory too long for the longest socket path, and a
    /// temporary directory on macOS is far longer.
    pub fn start(root: &Path, digest: &str, starter: Starter, data_dir: Option<PathBuf>) -> Self {
        let socket = root.join("launcher.sock");
        let short = tempfile::Builder::new()
            .prefix("ak")
            .rand_bytes(4)
            .tempdir_in("/tmp")
            .unwrap();
        let socket_dir = short.path().to_path_buf();
        let state = Arc::new(Mutex::new(State {
            digest: digest.to_owned(),
            starter,
            data_dir,
            socket_dir: socket_dir.clone(),
            _socket_dir: short,
            running: HashMap::new(),
            stopped_through: HashMap::new(),
            created_through: HashMap::new(),
            calls: Vec::new(),
            fail_ensure: None,
            fail_usage: None,
            unprove_stop: false,
            tokens_made: 0,
        }));
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(accept_loop(listener, state.clone()));
        Self {
            socket,
            socket_dir,
            state,
        }
    }

    /// Every request so far, as `"<type> <ws> [<gen>]"`.
    pub fn calls(&self) -> Vec<String> {
        lock(&self.state).calls.clone()
    }

    /// The next `ensure` answers `error`.
    pub fn fail_next_ensure(&self, code: LauncherErrorCode) {
        lock(&self.state).fail_ensure = Some(LauncherError {
            code,
            message: "scripted refusal".to_owned(),
        });
    }

    /// The next `usage` answers `error`.
    pub fn fail_next_usage(&self, code: LauncherErrorCode) {
        lock(&self.state).fail_usage = Some(LauncherError {
            code,
            message: "scripted refusal".to_owned(),
        });
    }

    /// The next `stop` removes the container but answers `proven: false`
    /// and records nothing, like a launcher whose proof listing failed.
    pub fn unprove_next_stop(&self) {
        lock(&self.state).unprove_stop = true;
    }

    /// The machine directory of `ws` at `generation`.
    pub fn machine_dir(&self, ws: &str, generation: u64) -> PathBuf {
        self.socket_dir.join(machine_dir_name(ws, generation))
    }

    /// The channel token of the workspace's container, if it has one.
    pub fn token(&self, ws: &str) -> Option<String> {
        lock(&self.state).running.get(ws).map(|c| c.token.clone())
    }

    /// End the workspace's container as if its supervisor died (the
    /// container stays, exited, until a `stop` removes it).
    pub fn kill(&self, ws: &str) {
        if let Some(container) = lock(&self.state).running.get(ws) {
            container.task.abort();
        }
    }
}

async fn accept_loop(listener: UnixListener, state: Arc<Mutex<State>>) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(serve(stream, state.clone()));
    }
}

async fn serve(mut stream: UnixStream, state: Arc<Mutex<State>>) {
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0; 16 * 1024];
    loop {
        while let Ok(Some(frame)) = decoder.next_frame() {
            let Frame::Control(json) = frame else { return };
            let Ok(request) = parse_control::<RequestEnvelope>(&json) else {
                return;
            };
            let reply = handle(&state, request.body).await;
            let frame = encode_control(&Envelope::new(request.id, reply)).unwrap();
            if stream.write_all(&frame).await.is_err() {
                return;
            }
        }
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(read) => decoder.push(&buf[..read]),
        }
    }
}

fn refusal(code: LauncherErrorCode) -> Reply {
    Reply::Error(LauncherError {
        code,
        message: "fake launcher refusal".to_owned(),
    })
}

async fn handle(state: &Mutex<State>, request: Request) -> Reply {
    match request {
        Request::Ensure {
            workspace_id,
            generation,
            image_digest,
            ..
        } => ensure(state, &workspace_id, generation, &image_digest),
        Request::Stop {
            workspace_id,
            generation,
            grace_ms,
        } => stop(state, &workspace_id, generation, grace_ms).await,
        Request::Inspect { workspace_id } => {
            let state = lock(state);
            Reply::Inspected {
                running: live(&state, &workspace_id),
                stopped_through: state.stopped_through.get(&workspace_id).copied(),
            }
        }
        Request::Usage { workspace_id } => usage(state, &workspace_id),
        Request::ResetHome { workspace_id } => volumes(state, &workspace_id, false),
        Request::Destroy { workspace_id } => volumes(state, &workspace_id, true),
    }
}

/// The generation whose container still runs.
fn live(state: &State, ws: &str) -> Option<u64> {
    state
        .running
        .get(ws)
        .filter(|c| !c.task.is_finished())
        .map(|c| c.generation)
}

/// A token no other container of this launcher got.
fn new_token(state: &mut State) -> String {
    state.tokens_made += 1;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    format!("fake{:08x}{:016x}", nanos, state.tokens_made)
}

fn ensure(state: &Mutex<State>, ws: &str, generation: u64, digest: &str) -> Reply {
    let mut state = lock(state);
    state.calls.push(format!("ensure {ws} {generation}"));
    if let Some(error) = state.fail_ensure.take() {
        return Reply::Error(error);
    }
    if digest != state.digest {
        return refusal(LauncherErrorCode::ImageMismatch);
    }
    if let Err(code) = admit(&state, ws, generation) {
        return refusal(code);
    }
    if let Some(container) = state.running.get(ws) {
        return Reply::Ensured {
            container_id: format!("c-{ws}-{generation}"),
            channel_token: container.token.clone(),
        };
    }
    let dir = state.socket_dir.join(machine_dir_name(ws, generation));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711)).unwrap();
    state.created_through.insert(ws.to_owned(), generation);
    let token = new_token(&mut state);
    let start = Start {
        workspace_id: ws.to_owned(),
        generation,
        dir,
        channel_token: token.clone(),
    };
    let task = (state.starter)(start);
    let container = Container {
        generation,
        task,
        token: token.clone(),
    };
    state.running.insert(ws.to_owned(), container);
    Reply::Ensured {
        container_id: format!("c-{ws}-{generation}"),
        channel_token: token,
    }
}

/// The real launcher's `decide`: `Ok` when the generation's container
/// runs (answer it) or nothing of the workspace exists (create it).
fn admit(state: &State, ws: &str, generation: u64) -> Result<(), LauncherErrorCode> {
    let stopped = state.stopped_through.get(ws).copied().unwrap_or(0);
    let created = state.created_through.get(ws).copied().unwrap_or(0);
    let existing = state.running.get(ws);
    let highest = existing.map_or(0, |c| c.generation);
    if generation <= stopped || generation < created || generation < highest {
        let current = highest.max(stopped).max(created);
        return Err(LauncherErrorCode::StaleGeneration { current });
    }
    match existing {
        Some(c) if c.generation != generation => Err(LauncherErrorCode::Busy),
        Some(c) if c.task.is_finished() => Err(LauncherErrorCode::Busy),
        Some(_) => Ok(()),
        None if generation <= created => Err(LauncherErrorCode::Busy),
        None => Ok(()),
    }
}

/// Wait up to the grace for the container of `generation` or a lower one
/// to end, then kill it; the generation (and every lower one) is then
/// proven stopped, whether it ran or not.
async fn stop(state: &Mutex<State>, ws: &str, generation: u64, grace_ms: u64) -> Reply {
    let container = {
        let mut state = lock(state);
        state.calls.push(format!("stop {ws} {generation}"));
        match state.running.get(ws) {
            Some(c) if c.generation <= generation => state.running.remove(ws),
            _ => None,
        }
    };
    if let Some(Container { mut task, .. }) = container {
        let grace = Duration::from_millis(grace_ms);
        if tokio::time::timeout(grace, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
    let mut state = lock(state);
    if std::mem::take(&mut state.unprove_stop) {
        return Reply::Stopped {
            proven: false,
            reason: Some("scripted: the listing failed".to_owned()),
        };
    }
    let proven = state.stopped_through.entry(ws.to_owned()).or_insert(0);
    *proven = (*proven).max(generation);
    Reply::Stopped {
        proven: true,
        reason: None,
    }
}

fn usage(state: &Mutex<State>, ws: &str) -> Reply {
    let mut state = lock(state);
    state.calls.push(format!("usage {ws}"));
    if let Some(error) = state.fail_usage.take() {
        return Reply::Error(error);
    }
    let (workspace_bytes, home_bytes) = match &state.data_dir {
        Some(data) => (
            tree_bytes(&data.join(ws).join("workspace")),
            tree_bytes(&data.join(ws).join("home")),
        ),
        None => (0, 0),
    };
    Reply::Usage {
        workspace_bytes,
        home_bytes,
    }
}

fn volumes(state: &Mutex<State>, ws: &str, destroy: bool) -> Reply {
    let mut state = lock(state);
    let name = if destroy { "destroy" } else { "reset_home" };
    state.calls.push(format!("{name} {ws}"));
    if live(&state, ws).is_some() {
        return refusal(LauncherErrorCode::Busy);
    }
    if let Some(data) = &state.data_dir {
        let root = data.join(ws);
        let _ = std::fs::remove_dir_all(root.join("home"));
        if destroy {
            let _ = std::fs::remove_dir_all(root.join("workspace"));
        } else {
            std::fs::create_dir_all(root.join("home")).unwrap();
        }
    }
    Reply::Done
}

/// Apparent bytes of the regular files below `dir`.
pub fn tree_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => tree_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
}
