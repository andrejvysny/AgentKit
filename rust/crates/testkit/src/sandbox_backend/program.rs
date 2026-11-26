//! What a scripted command does inside the fake guest. A test maps shell
//! text to a [`Program`] with `FakeSandboxBackend::script`; the fake runs
//! no real process, it plays the program against its virtual clock.

use agentkit_workspaces::RelPath;

/// Exit status of a connection the network policy refused (curl's
/// "couldn't connect").
pub const CONNECT_REFUSED_EXIT: i32 = 7;
/// Exit status of a command with no script ("command not found").
pub const UNSCRIPTED_EXIT: i32 = 127;
/// Exit status of a command that ran out of disk (fault `fill_disk`).
pub const DISK_FULL_EXIT: i32 = 28;

/// How a program ends once its run time has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramEnd {
    /// Exits with this status (0 is success).
    Exit(i32),
    /// Is killed by this signal.
    Signal(i32),
}

/// A scripted command. Built from one constructor and refined with the
/// chained setters; every field is applied at the program's first step
/// except the end, which waits for the run time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub(super) stdout: Vec<u8>,
    pub(super) end: ProgramEnd,
    /// Virtual milliseconds until it ends; `None` runs until cancelled or
    /// its deadline.
    pub(super) run_ms: Option<u64>,
    /// Printed again and again until the output limit ends the program.
    pub(super) endless: Option<Vec<u8>>,
    /// A connection attempted at the first step (or later, see
    /// [`Program::connects_after`]).
    pub(super) connect: Option<(String, u16)>,
    /// Virtual milliseconds after the start the connection is attempted.
    pub(super) connect_after_ms: u64,
    /// Leaves a process behind; the fake ends it with the command.
    pub(super) background: bool,
    /// Bytes appended to workspace files when the program exits 0.
    pub(super) appends: Vec<(RelPath, Vec<u8>)>,
}

impl Program {
    /// Ends at once with `code`.
    pub fn exit(code: i32) -> Self {
        Self {
            stdout: Vec::new(),
            end: ProgramEnd::Exit(code),
            run_ms: Some(0),
            endless: None,
            connect: None,
            connect_after_ms: 0,
            background: false,
            appends: Vec::new(),
        }
    }

    /// Ends at once, killed by `signal`.
    pub fn killed_by(signal: i32) -> Self {
        Self {
            end: ProgramEnd::Signal(signal),
            ..Self::exit(0)
        }
    }

    /// Runs until cancelled or its deadline passes (`sleep 3600`).
    pub fn hang() -> Self {
        Self {
            run_ms: None,
            ..Self::exit(0)
        }
    }

    /// Prints `chunk` without end (`yes`): the output limit ends it as
    /// `OutputLimit`. Only bytes up to the limit are ever held.
    ///
    /// # Panics
    /// When `chunk` is empty.
    pub fn endless(chunk: impl AsRef<[u8]>) -> Self {
        let chunk = chunk.as_ref().to_vec();
        assert!(!chunk.is_empty(), "an endless printer needs bytes to print");
        Self {
            endless: Some(chunk),
            ..Self::hang()
        }
    }

    /// Connects to `host:port`, then exits 0. When the machine's network
    /// policy does not allow the target, the fake records a denial and the
    /// program fails with [`CONNECT_REFUSED_EXIT`].
    pub fn connect(host: &str, port: u16) -> Self {
        Self {
            connect: Some((host.to_owned(), port)),
            ..Self::exit(0)
        }
    }

    /// Attempts its connection `ms` virtual milliseconds after it started
    /// instead of at once, so a policy change made while it runs decides.
    pub fn connects_after(mut self, ms: u64) -> Self {
        self.connect_after_ms = ms;
        self
    }

    /// Prints `bytes` at its first step.
    pub fn stdout(mut self, bytes: impl AsRef<[u8]>) -> Self {
        self.stdout = bytes.as_ref().to_vec();
        self
    }

    /// Ends `ms` virtual milliseconds after it started.
    pub fn runs_for(mut self, ms: u64) -> Self {
        self.run_ms = Some(ms);
        self
    }

    /// Leaves a background process (`cmd &`). The fake ends it with the
    /// command, so nothing it would do later ever happens (F-7).
    pub fn leave_background(mut self) -> Self {
        self.background = true;
        self
    }

    /// Appends `bytes` to `path` when the program exits 0.
    ///
    /// # Panics
    /// When `path` is not a valid relative path.
    pub fn append(mut self, path: &str, bytes: impl AsRef<[u8]>) -> Self {
        let path = RelPath::parse(path).expect("append path is a valid relative path");
        self.appends.push((path, bytes.as_ref().to_vec()));
        self
    }

    /// The program run for shell text nobody scripted.
    pub(super) fn unscripted(command: &str) -> Self {
        Self::exit(UNSCRIPTED_EXIT).stdout(format!("fake: no script for `{command}`\n"))
    }
}
