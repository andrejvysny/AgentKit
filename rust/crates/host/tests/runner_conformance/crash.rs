use super::support::*;
use agentkit_store::SqliteStore;
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub async fn child(path: String) {
    let env = Environment::new(SqliteStore::open(path, 16).unwrap(), "crash_recovery", 1000).await;
    env.submit().await;
    env.host.run_once().await.unwrap();
    let task = env.host.store().get_task("run").await.unwrap().unwrap();
    assert!(task.retry_pending);
    assert_eq!(task.available_at, 1200);
    std::fs::write(
        std::env::var("AGENTKIT_RUNNER_CHILD_READY").unwrap(),
        b"ready",
    )
    .unwrap();
    loop {
        std::thread::park();
    }
}

async fn kill_at_retry(path: &Path, ready: &Path) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runner_crash_child", "--nocapture"])
        .env("AGENTKIT_RUNNER_CHILD_DB", path)
        .env("AGENTKIT_RUNNER_CHILD_READY", ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() && Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "crash child exited before retry boundary"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let became_ready = ready.exists();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(became_ready, "crash child never reached retry boundary");
}

pub async fn run(directory: &Path) -> Vec<Value> {
    let path = directory.join("native-crash.db");
    kill_at_retry(&path, &directory.join("native-ready")).await;
    let first = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "crash_recovery",
        2000,
    )
    .await;
    let mut transcript = vec![snapshot(&first, "recovered", json!({})).await];
    assert!(!first.host.run_once().await.unwrap());
    drop(first);
    let reopened = Environment::new(
        SqliteStore::open(&path, 16).unwrap(),
        "crash_recovery",
        2100,
    )
    .await;
    transcript.push(snapshot(&reopened, "reopened_again", json!({})).await);
    reopened.host.recover().await.unwrap();
    transcript.push(snapshot(&reopened, "recovered_again", json!({})).await);
    assert!(!reopened.host.run_once().await.unwrap());
    reopened.at(2200);
    reopened.host.resume("run").await.unwrap();
    assert!(reopened.host.run_once().await.unwrap());
    transcript.push(snapshot(&reopened, "settled", json!({})).await);
    transcript
}
