//! One operation inside the fake guest and how it progresses. Progress is
//! lazy: every port call that touches a machine first steps its operations
//! against the virtual clock, so a test moves a program on by advancing
//! the clock and calling the port.

use agentkit_workspaces::{
    ByteSource, Denial, FileMode, HostPort, NetworkPolicy, Observation, OperationId, OperationKind,
    OperationRequest, OperationState, OutputLimit, PathRoot, RelPath, Sha256,
};

use super::program::{CONNECT_REFUSED_EXIT, DISK_FULL_EXIT, Program, ProgramEnd};
use super::volume::{File, Volume, WriteRefusal};

/// What one step may touch outside the operation.
pub(super) struct Env<'a> {
    /// `/workspace`.
    pub(super) volume: &'a mut Volume,
    /// `/home/agent`; shares the quota with `volume`.
    pub(super) home: &'a mut Volume,
    pub(super) quota: u64,
    pub(super) disk_full: bool,
    pub(super) policy: &'a NetworkPolicy,
    pub(super) denials: &'a mut Vec<Denial>,
    pub(super) background_ended: &'a mut u32,
    pub(super) now: i64,
}

enum Work {
    Exec {
        program: Program,
        deadline_at: i64,
        limit: OutputLimit,
        begun: bool,
        /// The program's connection is still to come.
        connect_pending: bool,
    },
    Write {
        root: PathRoot,
        path: RelPath,
        sha256: Sha256,
        bytes: Vec<u8>,
        mode: FileMode,
    },
}

fn exec_work(program: Program, timeout_ms: u64, limit: OutputLimit, now: i64) -> Work {
    Work::Exec {
        program,
        deadline_at: now.saturating_add(i64::try_from(timeout_ms).unwrap_or(i64::MAX)),
        limit,
        begun: false,
        connect_pending: false,
    }
}

pub(super) struct FakeOp {
    pub(super) id: OperationId,
    pub(super) args: Sha256,
    pub(super) state: OperationState,
    started_at: i64,
    output: Vec<u8>,
    lines: u64,
    truncated: bool,
    work: Work,
}

impl FakeOp {
    /// A new operation; `program` is what an exec runs.
    pub(super) fn new(req: &OperationRequest, program: Option<Program>, now: i64) -> Self {
        let (state, work) = match (&req.kind, program) {
            (
                OperationKind::Exec {
                    timeout_ms,
                    output_limit,
                    ..
                },
                Some(program),
            ) => (
                OperationState::Running,
                exec_work(program, *timeout_ms, *output_limit, now),
            ),
            (
                OperationKind::WriteFile {
                    root,
                    path,
                    sha256,
                    bytes: ByteSource::Inline(bytes),
                    mode,
                },
                _,
            ) => (
                OperationState::Accepted,
                Work::Write {
                    root: *root,
                    path: path.clone(),
                    sha256: sha256.clone(),
                    bytes: bytes.clone(),
                    mode: *mode,
                },
            ),
            (OperationKind::Exec { .. }, None) => unreachable!("an exec always has a program"),
        };
        Self {
            id: req.id.clone(),
            args: req.args_sha256.clone(),
            state,
            started_at: now,
            output: Vec::new(),
            lines: 0,
            truncated: false,
            work,
        }
    }

    /// Bytes of output the guest holds; never more than the limit.
    pub(super) fn output_len(&self) -> u64 {
        self.output.len() as u64
    }

    /// Output after `after_seq`, at most `max` bytes.
    pub(super) fn observe(&self, after_seq: u64, max: u64) -> Observation {
        let len = self.output_len();
        let start = after_seq.min(len);
        let end = start + max.min(len - start);
        Observation {
            state: self.state,
            output: self.output[start as usize..end as usize].to_vec(),
            next_seq: after_seq.max(end),
            truncated: self.truncated,
            eof: self.state.is_terminal() && end == len,
        }
    }

    /// End the whole process tree as `Cancelled`; a receipt stays as is.
    pub(super) fn cancel(&mut self, background_ended: &mut u32) {
        if !self.state.is_terminal() {
            self.end(OperationState::Cancelled, background_ended);
        }
    }

    /// Move the operation on to `env.now`.
    pub(super) fn step(&mut self, env: &mut Env<'_>) {
        if self.state.is_terminal() {
            return;
        }
        let outcome = match &self.work {
            Work::Write { .. } => Some(self.apply_write(env)),
            Work::Exec { .. } => self.step_exec(env),
        };
        if let Some(state) = outcome {
            self.end(state, env.background_ended);
        }
    }

    fn apply_write(&mut self, env: &mut Env<'_>) -> OperationState {
        let Work::Write {
            root,
            path,
            sha256,
            bytes,
            mode,
        } = &self.work
        else {
            unreachable!("apply_write runs on writes only")
        };
        if Sha256::of(bytes) != *sha256 {
            return failed(1);
        }
        if env.disk_full {
            return failed(DISK_FULL_EXIT);
        }
        let file = File {
            bytes: bytes.clone(),
            mtime_ms: env.now,
            mode: *mode,
        };
        let (tree, quota) = env.tree(*root);
        match tree.write(path, file, quota) {
            Ok(()) => OperationState::Succeeded { exit: 0 },
            Err(WriteRefusal::NotAFile) => failed(1),
            Err(WriteRefusal::Quota) => failed(DISK_FULL_EXIT),
        }
    }

    fn step_exec(&mut self, env: &mut Env<'_>) -> Option<OperationState> {
        let Work::Exec {
            program,
            deadline_at,
            begun,
            connect_pending,
            ..
        } = &mut self.work
        else {
            unreachable!("step_exec runs on commands only")
        };
        let (program, deadline_at) = (program.clone(), *deadline_at);
        let connect_at = self
            .started_at
            .saturating_add(i64::try_from(program.connect_after_ms).unwrap_or(i64::MAX));
        let first = !*begun;
        *begun = true;
        if first {
            *connect_pending = program.connect.is_some();
        }
        let connect_now = *connect_pending && env.now >= connect_at;
        if connect_now {
            *connect_pending = false;
            if let Some(state) = self.connect(&program, env) {
                return Some(state);
            }
        }
        if first && let Some(state) = self.begin(&program) {
            return Some(state);
        }
        if env.disk_full {
            return Some(failed(DISK_FULL_EXIT));
        }
        let finish_at = program.run_ms.map(|ms| {
            self.started_at
                .saturating_add(i64::try_from(ms).unwrap_or(i64::MAX))
        });
        match finish_at {
            Some(at) if at <= env.now && at < deadline_at => Some(finish(&program, env)),
            _ if env.now >= deadline_at => Some(OperationState::TimedOut),
            _ => None,
        }
    }

    /// The connection, against the machine's policy at this step. Answers
    /// a state when the policy refused it (the program fails).
    fn connect(&mut self, program: &Program, env: &mut Env<'_>) -> Option<OperationState> {
        let (host, port) = program.connect.as_ref()?;
        let allowed = HostPort::new(host, *port).is_ok_and(|target| env.policy.allows(&target));
        if allowed {
            return None;
        }
        env.denials.push(Denial {
            host: host.clone(),
            port: *port,
            operation_id: Some(self.id.clone()),
            at: env.now,
        });
        let message = format!("fake: connection to {host}:{port} refused by policy\n");
        Some(
            self.print(message.as_bytes())
                .unwrap_or(failed(CONNECT_REFUSED_EXIT)),
        )
    }

    /// The first step after the connection: print. Answers a state when
    /// that ends it.
    fn begin(&mut self, program: &Program) -> Option<OperationState> {
        if let Some(state) = self.print(&program.stdout) {
            return Some(state);
        }
        let chunk = program.endless.as_deref()?;
        loop {
            if let Some(state) = self.print(chunk) {
                return Some(state);
            }
        }
    }

    /// Keep `bytes` up to the output limit. Answers `OutputLimit` when a
    /// byte had to be dropped; nothing past the limit is ever held.
    fn print(&mut self, bytes: &[u8]) -> Option<OperationState> {
        let Work::Exec { limit, .. } = &self.work else {
            return None;
        };
        let limit = *limit;
        for &byte in bytes {
            let full = self.output_len() >= limit.max_bytes
                || limit.max_lines.is_some_and(|max| self.lines >= max);
            if full {
                self.truncated = true;
                return Some(OperationState::OutputLimit);
            }
            self.output.push(byte);
            if byte == b'\n' {
                self.lines += 1;
            }
        }
        None
    }

    fn end(&mut self, state: OperationState, background_ended: &mut u32) {
        self.state = state;
        if let Work::Exec { program, .. } = &self.work
            && program.background
        {
            *background_ended += 1;
        }
    }
}

impl Env<'_> {
    /// The tree `root` names and the quota left for it: both trees share
    /// one volume quota.
    fn tree(&mut self, root: PathRoot) -> (&mut Volume, u64) {
        match root {
            PathRoot::Workspace => {
                let left = self.quota.saturating_sub(self.home.used_bytes());
                (&mut *self.volume, left)
            }
            PathRoot::Home => {
                let left = self.quota.saturating_sub(self.volume.used_bytes());
                (&mut *self.home, left)
            }
        }
    }
}

/// The receipt of a program that reached its run time.
fn finish(program: &Program, env: &mut Env<'_>) -> OperationState {
    match program.end {
        ProgramEnd::Exit(0) => {
            let now = env.now;
            for (path, bytes) in &program.appends {
                let (tree, quota) = env.tree(PathRoot::Workspace);
                if !tree.append(path, bytes, now, quota) {
                    return failed(1);
                }
            }
            OperationState::Succeeded { exit: 0 }
        }
        ProgramEnd::Exit(code) => failed(code),
        ProgramEnd::Signal(signal) => OperationState::Failed {
            exit: None,
            signal: Some(signal),
        },
    }
}

fn failed(code: i32) -> OperationState {
    OperationState::Failed {
        exit: Some(code),
        signal: None,
    }
}
