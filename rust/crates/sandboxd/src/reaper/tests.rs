use std::os::unix::process::CommandExt;

use agentkit_sys::{KillProbe, kill_probe};

use super::*;

fn sleeper() -> Command {
    let mut cmd = Command::new("/bin/sh");
    // The background sleep stays in the leader's process group.
    cmd.args(["-c", "sleep 3600 & echo $! > /dev/null; exec sleep 3600"]);
    cmd.process_group(0);
    cmd
}

#[tokio::test]
async fn an_ended_operation_takes_its_group_and_releases_the_leader() {
    let reaper = Reaper::new(ProcessMode::Embedded);
    let pid = reaper.spawn(&mut sleeper()).unwrap();
    let group = ProcessGroup::from_leader_pid(pid).unwrap();
    assert!(reaper.poll_exit(pid).is_none(), "still running");
    reaper.end_operation(pid).await;
    assert!(group.is_gone(), "the whole group ended");
    let probe = i32::try_from(pid).unwrap();
    assert_eq!(kill_probe(probe), KillProbe::Gone, "the leader was reaped");
    assert!(reaper.lock().slots.is_empty(), "released");
}

#[tokio::test]
async fn an_exit_is_seen_without_reaping_until_release() {
    let reaper = Reaper::new(ProcessMode::Embedded);
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "exit 5"]);
    let pid = reaper.spawn(&mut cmd).unwrap();
    assert_eq!(reaper.wait_exit(pid).await, ChildExit::Exited { code: 5 });
    let probe = i32::try_from(pid).unwrap();
    assert_eq!(
        kill_probe(probe),
        KillProbe::Alive,
        "a zombie holds the pid"
    );
    reaper.end_operation(pid).await;
    assert_eq!(kill_probe(probe), KillProbe::Gone);
}

#[tokio::test]
async fn nothing_starts_after_shutdown_began() {
    let reaper = Reaper::new(ProcessMode::Embedded);
    let pid = reaper.spawn(&mut sleeper()).unwrap();
    reaper.end_all().await;
    assert!(matches!(
        reaper.wait_exit(pid).await,
        ChildExit::Killed { .. }
    ));
    reaper.end_operation(pid).await;
    let refused = reaper.spawn(&mut sleeper());
    assert!(refused.is_err(), "a spawn after end_all is refused");
}

#[tokio::test]
async fn a_cancelled_leader_gets_sigterm_first() {
    let reaper = Reaper::new(ProcessMode::Embedded);
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("3600").process_group(0);
    let pid = reaper.spawn(&mut cmd).unwrap();
    reaper.terminate(pid).await;
    assert_eq!(
        reaper.wait_exit(pid).await,
        ChildExit::Killed {
            signal: Signal::Term.number()
        }
    );
    reaper.end_operation(pid).await;
}

#[test]
fn only_a_platform_without_proc_spares_the_descendant_sweep() {
    let error = |kind| io::Error::new(kind, "scan");
    assert!(!scan_failure_is_fatal(&error(io::ErrorKind::Unsupported)));
    for kind in [
        io::ErrorKind::NotFound,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::Other,
    ] {
        assert!(scan_failure_is_fatal(&error(kind)), "{kind:?}");
    }
}
