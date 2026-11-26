//! No descriptor of the supervisor reaches a command: the control socket,
//! logs, pipes and directories it holds are all close-on-exec. Alone in
//! its binary, so no concurrent test opens descriptors while it compares.

mod support;

use std::process::{Command, Stdio};

use agentkit_sandbox_proto::guest::OperationState;
use support::{Guest, exec_request, start};

/// Prints every open descriptor from 3 up, using only shell redirection
/// (a lister would open descriptors of its own).
const PROBE: &str =
    "for fd in $(seq 3 255); do { true >&$fd; } 2>/dev/null && printf '%s ' $fd; done; true";

#[tokio::test]
async fn only_inherited_descriptors_reach_the_command() {
    // What any child of this process inherits anyway (the test runner may
    // pass descriptors down without close-on-exec), before the supervisor
    // opens anything.
    let baseline = Command::new("/bin/bash")
        .args(["-c", PROBE])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let baseline = String::from_utf8(baseline.stdout).unwrap();
    let guest = Guest::new("PATH=/usr/bin:/bin\n");
    let mut host = start(&guest).await;
    let probe = host.run(exec_request("op-1", PROBE)).await;
    assert_eq!(probe.state, OperationState::Succeeded { exit: 0 });
    assert_eq!(
        String::from_utf8(probe.output).unwrap(),
        baseline,
        "the command sees a descriptor the supervisor opened"
    );
}
