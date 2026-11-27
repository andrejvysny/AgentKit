//! A launcher restart: helpers of the earlier run that are still ours are
//! SIGKILLed and proven gone, helpers that ended on their own are found
//! gone, and both generations are recorded stopped.

mod support;

use std::time::{Duration, Instant};

use agentkit_sandbox_proto::launcher::LauncherErrorCode;
use support::*;

/// Wait until `pid` is gone (the earlier launcher's waiter reaps it).
async fn gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while agentkit_sys::pid_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_kills_leftover_helpers_and_proves_their_generations_stopped() {
    let fixture = Fixture::new(NORMAL);
    let first = fixture.start().await;
    let digest = first.image_digest().to_owned();
    let (stay, _) = ensured(call(&first, ensure("ws-stay", 1, &digest)).await);
    let (quit, _) = ensured(call(&first, ensure("ws-quit", 2, &digest)).await);
    let (stay, quit) = (pid_of(&stay), pid_of(&quit));
    first.close().await;
    assert!(gone(quit).await, "a helper stops on EOF of its stdin");
    assert!(agentkit_sys::pid_alive(stay), "ws-stay ignores EOF");
    let second = fixture.start().await;
    assert!(gone(stay).await, "recovery killed the leftover helper");
    for (ws, generation) in [("ws-stay", 1), ("ws-quit", 2)] {
        let state = inspected(call(&second, inspect(ws)).await);
        assert_eq!(state, (None, Some(generation)), "{ws}");
        let stale = LauncherErrorCode::StaleGeneration {
            current: generation,
        };
        let again = call(&second, ensure(ws, generation, &digest)).await;
        assert_eq!(refusal(again), stale, "{ws}");
    }
    ensured(call(&second, ensure("ws-quit", 3, &digest)).await);
    second.shutdown().await.unwrap();
}
