//! The read tools: bounded file reads with a continuation offset, binary
//! files by type and size, listings relative to the working folder, and a
//! status that creates nothing for a never-used attachment (F-16) and names
//! a fence.

mod support;

use agentkit_toolpacks_sandbox::SandboxMode;
use agentkit_workspaces::WorkspaceRepository;
use serde_json::json;
use support::{World, call, chat_binding, data, text};

#[tokio::test]
async fn a_read_shows_text_and_where_to_continue() {
    let binding = chat_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    let body = "a\n".repeat(3_000);
    world.put(&binding, "big.txt", &body).await;
    let tools = world.stage().await;
    let first = tools["sandbox_read_file"]
        .execute(call("r1", None), json!({"file_path": "big.txt"}))
        .await
        .unwrap();
    assert!(first.ok, "{}", text(&first));
    assert_eq!(data(&first)["nextOffset"], 4_000, "2,000 lines");
    assert!(text(&first).ends_with("[bytes 0 to 4000 of 6000; continue with offset 4000]"));
    let rest = tools["sandbox_read_file"]
        .execute(
            call("r2", None),
            json!({"file_path": "/workspace/chats/c1/big.txt", "offset": 4000}),
        )
        .await
        .unwrap();
    assert_eq!(data(&rest)["bytes"], 2_000);
    assert!(data(&rest).get("nextOffset").is_none());
}

#[tokio::test]
async fn a_binary_file_reports_type_and_size_only() {
    let binding = chat_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "blob.bin", "a\u{0}b").await;
    let tools = world.stage().await;
    let read = tools["sandbox_read_file"]
        .execute(call("r1", None), json!({"file_path": "blob.bin"}))
        .await
        .unwrap();
    assert!(read.ok);
    assert_eq!(data(&read)["binary"], true);
    assert!(text(&read).starts_with("/workspace/chats/c1/blob.bin is a binary file of 3 bytes"));
}

#[tokio::test]
async fn a_listing_is_relative_to_the_folder_and_marks_folders() {
    let binding = chat_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "input/data.csv", "x,y\n").await;
    world.put(&binding, "notes.md", "n\n").await;
    let tools = world.stage().await;
    let listed = tools["sandbox_list_files"]
        .execute(call("l1", None), json!({"depth": 2}))
        .await
        .unwrap();
    let shown = text(&listed);
    assert!(
        shown.starts_with("/workspace/chats/c1 (depth 2):"),
        "{shown}"
    );
    for line in ["input/", "input/data.csv  4 B", "notes.md  2 B"] {
        assert!(shown.lines().any(|l| l == line), "{line} in {shown}");
    }
}

#[tokio::test]
async fn status_of_a_never_used_attachment_creates_nothing() {
    let world = World::new(Some(chat_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    let status = tools["sandbox_get_status"]
        .execute(call("s1", None), json!({}))
        .await
        .unwrap();
    assert!(status.ok);
    assert!(text(&status).starts_with("Sandbox: not created yet"));
    assert!(text(&status).contains("Working folder: /workspace/chats/c1"));
    assert!(text(&status).contains("Image: 2026.11 (sha256:image-1) (Debian GNU/Linux (slim))"));
    assert!(
        world.repo.list_workspaces().await.unwrap().is_empty(),
        "F-16"
    );
    assert_eq!(world.fake.machines_started(), 0);
}

#[tokio::test]
async fn status_names_the_hosts_and_a_fence() {
    let binding = chat_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "a.txt", "x").await;
    let tools = world.stage().await;
    let status = tools["sandbox_get_status"]
        .execute(call("s1", None), json!({}))
        .await
        .unwrap();
    assert!(
        text(&status).contains("Allowed hosts: pypi.org:443"),
        "{}",
        text(&status)
    );
    assert_eq!(data(&status)["state"]["state"], "ready");
    world.fake.refuse_stop(true).lose_next_ack();
    tools["sandbox_run"]
        .execute(call("c1", Some("e1")), json!({"command": "echo hi"}))
        .await
        .unwrap();
    let fenced = tools["sandbox_get_status"]
        .execute(call("s2", None), json!({}))
        .await
        .unwrap();
    assert!(fenced.ok);
    assert!(
        text(&fenced)
            .contains("sandbox_fenced: nothing runs in this sandbox until the owner recovers it"),
        "{}",
        text(&fenced)
    );
    let read = tools["sandbox_read_file"]
        .execute(call("r1", None), json!({"file_path": "a.txt"}))
        .await
        .unwrap();
    assert_eq!(data(&read)["errorCode"], "sandbox_fenced");
}

/// Repeated slashes name the same file; an escape is still refused.
#[tokio::test]
async fn repeated_slashes_name_the_same_file() {
    let binding = chat_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    world.put(&binding, "out/report.csv", "north,42\n").await;
    let tools = world.stage().await;
    for (n, path) in [
        "/workspace//chats/c1/out/report.csv",
        "//workspace/chats//c1///out/report.csv",
        "out//report.csv",
    ]
    .into_iter()
    .enumerate()
    {
        let read = tools["sandbox_read_file"]
            .execute(call(&format!("r{n}"), None), json!({"file_path": path}))
            .await
            .unwrap();
        assert!(read.ok, "{path}: {}", text(&read));
    }
    let escape = tools["sandbox_read_file"]
        .execute(
            call("r9", None),
            json!({"file_path": "/workspace//chats/c1/..//../c2/x"}),
        )
        .await
        .unwrap();
    assert_eq!(data(&escape)["errorCode"], "path_outside_workspace");
}
