//! The shell text the suite runs, and the fake's script for each, side by
//! side so the two never drift apart. A real backend runs the text through
//! the image's shell; the fake plays the program it is mapped to.

use std::sync::Arc;

use agentkit_workspaces::WorkspaceId;
use async_trait::async_trait;

use super::{SandboxDriver, SandboxHarness};
use crate::{FakeSandboxBackend, Program};

/// Prints `hi` and exits 0.
pub const PRINT_HI: &str = "printf hi";
/// Exits 3.
pub const EXIT_3: &str = "exit 3";
/// Runs until cancelled or its deadline.
pub const SLEEP: &str = "sleep 3600";
/// Prints without end.
pub const YES: &str = "yes";
/// Appends `x` to `mark.txt`: shows how often a command really ran.
pub const APPEND_MARK: &str = "printf x >> mark.txt";
/// Leaves a process that would write `late.txt` a second later, and exits
/// 0. The backend must end it with the command (F-7).
pub const BACKGROUND: &str = "(sleep 1; printf late > late.txt) & exit 0";
/// Connects to [`CONNECT_HOST`]:[`CONNECT_PORT`]; fails when the policy
/// refuses it.
pub const CONNECT: &str = "curl -fsS -o /dev/null --max-time 10 https://example.com/";
/// Host [`CONNECT`] reaches.
pub const CONNECT_HOST: &str = "example.com";
/// Port [`CONNECT`] reaches.
pub const CONNECT_PORT: u16 = 443;

/// Map every suite command to its program on `fake`.
pub fn script_fake(fake: &FakeSandboxBackend) {
    fake.script(PRINT_HI, Program::exit(0).stdout("hi"))
        .script(EXIT_3, Program::exit(3))
        .script(SLEEP, Program::hang())
        .script(YES, Program::endless("y\n"))
        .script(APPEND_MARK, Program::exit(0).append("mark.txt", "x"))
        .script(BACKGROUND, Program::exit(0).leave_background())
        .script(CONNECT, Program::connect(CONNECT_HOST, CONNECT_PORT));
}

#[async_trait]
impl SandboxDriver for FakeSandboxBackend {
    async fn elapse(&self, ms: u64) {
        self.advance(ms);
    }
}

/// `fake` with the suite's scripts, as a harness.
pub fn fake_harness(fake: FakeSandboxBackend) -> SandboxHarness {
    script_fake(&fake);
    SandboxHarness {
        backend: Arc::new(fake.clone()),
        driver: Arc::new(fake),
        workspace_id: WorkspaceId::new("ws-1"),
        image_digest: "sha256:conformance-image".to_owned(),
    }
}
