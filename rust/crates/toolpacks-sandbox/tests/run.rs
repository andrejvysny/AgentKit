//! `sandbox_run` and `sandbox_write_file` against the fake backend: the
//! operation id is the admitted execution id (INV-2), and every outcome
//! has its envelope: a receipt, a refused connection, an unknown outcome,
//! a fenced or busy workspace, and refusals before the row (not started).

mod support;

use agentkit_toolpacks_sandbox::SandboxMode;
use agentkit_workspaces::{AgentId, OpContext, OperationId, Sha256};
use serde_json::json;
use support::{
    CURL, ECHO, EXIT_3, MARK, SLEEP, World, call, chat_binding, data, home_binding, not_started,
    text,
};

#[tokio::test]
async fn the_operation_id_is_the_admitted_execution_id() {
    let world = World::new(Some(chat_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    let result = tools["sandbox_run"]
        .execute(call("c1", Some("exec-42")), json!({"command": ECHO}))
        .await
        .unwrap();
    assert!(result.ok, "{}", text(&result));
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("exec-42")), 1);
    assert_eq!(world.fake.dispatch_calls_total(), 1);
    assert_eq!(data(&result)["operationId"], "exec-42");
    assert_eq!(text(&result), "hi\n[exit code 0]");
    let details = data(&result);
    assert_eq!(
        (details["exitCode"].clone(), details["state"].clone()),
        (json!(0), json!("succeeded"))
    );
    assert_eq!(details["logPath"], "/workspace/.sandbox/logs/exec-42.log");
    assert!(details["durationMs"].is_number());
}

#[tokio::test]
async fn without_an_admitted_execution_id_nothing_is_dispatched() {
    let world = World::new(Some(chat_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    for (tool, args) in [
        ("sandbox_run", json!({"command": MARK})),
        (
            "sandbox_write_file",
            json!({"file_path": "a", "content": "x"}),
        ),
    ] {
        let result = tools[tool].execute(call("c1", None), args).await.unwrap();
        assert!(not_started(&result), "{tool}");
        assert_eq!(data(&result)["code"], "durable_execution_required");
    }
    assert_eq!(world.fake.dispatch_calls_total(), 0);
}

#[tokio::test]
async fn a_non_zero_exit_and_a_timeout_are_receipts_not_errors() {
    let world = World::new(Some(home_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    let failed = tools["sandbox_run"]
        .execute(call("c1", Some("e1")), json!({"command": EXIT_3}))
        .await
        .unwrap();
    assert!(!failed.ok);
    assert_eq!(text(&failed), "bad input\n[exit code 3]");
    assert_eq!(data(&failed)["exitCode"], 3);
    assert!(data(&failed).get("errorCode").is_none());
    let late = tools["sandbox_run"]
        .execute(
            call("c2", Some("e2")),
            json!({"command": SLEEP, "timeout": 1000}),
        )
        .await
        .unwrap();
    assert!(!late.ok);
    assert_eq!(data(&late)["state"], "timed_out");
    assert!(text(&late).ends_with("[timed out: the sandbox ended the command at its deadline]"));
}

#[tokio::test]
async fn a_refused_connection_names_the_host_and_the_recorded_request() {
    let world = World::new(Some(home_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    let result = tools["sandbox_run"]
        .execute(call("c1", Some("e1")), json!({"command": CURL}))
        .await
        .unwrap();
    assert!(!result.ok);
    assert!(text(&result).contains("network_denied: the connection to example.com:443 was refused by the sandbox network policy. A request was recorded for the owner"), "{}", text(&result));
    assert_eq!(data(&result)["errorCode"], "network_denied");
    assert_eq!(
        data(&result)["networkDenied"],
        json!([{"host": "example.com", "port": 443}])
    );
}

#[tokio::test]
async fn a_lost_ack_is_an_unknown_outcome_and_an_unproven_stop_fences() {
    let world = World::new(Some(home_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    world.fake.lose_next_ack();
    let unknown = tools["sandbox_run"]
        .execute(call("c1", Some("e1")), json!({"command": MARK}))
        .await
        .unwrap();
    assert!(!unknown.ok);
    assert!(
        !not_started(&unknown),
        "may have started: never the not-started shape"
    );
    assert_eq!(data(&unknown)["errorCode"], "tool_result_uncertain");
    assert!(text(&unknown).starts_with("tool_result_uncertain: outcome unknown: OneMind stopped the sandbox before the command reported back."));
    assert_eq!(data(&unknown)["fenced"], false);
    // A fresh sandbox whose machine cannot be proven stopped.
    let world = World::new(Some(home_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    world.fake.refuse_stop(true).lose_next_ack();
    let fenced = tools["sandbox_run"]
        .execute(call("c2", Some("e2")), json!({"command": MARK}))
        .await
        .unwrap();
    assert!(!not_started(&fenced));
    assert_eq!(data(&fenced)["fenced"], true, "{}", text(&fenced));
    assert!(text(&fenced).contains("The sandbox is fenced now"));
    let refused = tools["sandbox_write_file"]
        .execute(
            call("c3", Some("e3")),
            json!({"file_path": "a", "content": "x"}),
        )
        .await
        .unwrap();
    assert!(not_started(&refused));
    assert_eq!(data(&refused)["code"], "sandbox_fenced");
    assert!(text(&refused).starts_with("sandbox_fenced: "));
}

#[tokio::test]
async fn a_held_workspace_answers_busy_naming_the_holder_and_runs_nothing() {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    let tools = world.stage().await;
    let service = world.service.clone();
    let holder = OpContext::new(
        OperationId::new("held"),
        Sha256::of(b"held"),
        "run-other",
        AgentId::new("a1"),
    );
    let cancel = holder.cancellation.clone();
    let held = tokio::spawn({
        let binding = binding.binding.clone();
        async move {
            service
                .exec(
                    &holder,
                    &binding,
                    agentkit_workspaces::ExecRequest::new(SLEEP),
                )
                .await
        }
    });
    world.until_busy().await;
    let busy = tools["sandbox_run"]
        .execute(call("c1", Some("e1")), json!({"command": ECHO}))
        .await
        .unwrap();
    assert!(not_started(&busy), "{}", text(&busy));
    assert_eq!(data(&busy)["code"], "sandbox_busy");
    assert_eq!(world.fake.dispatch_calls(&OperationId::new("e1")), 0);
    cancel.cancel();
    held.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_path_outside_the_folder_is_refused_before_anything_runs() {
    let world = World::new(Some(chat_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    for (tool, args) in [
        ("sandbox_run", json!({"command": ECHO, "directory": "/etc"})),
        ("sandbox_run", json!({"command": ECHO, "directory": "~/x"})),
        (
            "sandbox_write_file",
            json!({"file_path": "/workspace/chats/c2/a", "content": "x"}),
        ),
        (
            "sandbox_write_file",
            json!({"file_path": "../c2/a", "content": "x"}),
        ),
    ] {
        let result = tools[tool]
            .execute(call("c1", Some("e1")), args.clone())
            .await
            .unwrap();
        assert!(not_started(&result), "{args}");
        assert_eq!(data(&result)["code"], "path_outside_workspace", "{args}");
    }
    assert_eq!(world.fake.dispatch_calls_total(), 0);
}

#[tokio::test]
async fn writes_land_in_the_chat_folder_or_the_home_tree() {
    let binding = chat_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    let tools = world.stage().await;
    let first = tools["sandbox_write_file"]
        .execute(
            call("c1", Some("e1")),
            json!({"file_path": "/workspace/chats/c1/notes.md", "content": "# hi\n"}),
        )
        .await
        .unwrap();
    assert!(first.ok, "{}", text(&first));
    assert_eq!(
        text(&first),
        "Wrote 5 bytes to /workspace/chats/c1/notes.md."
    );
    assert_eq!(data(&first)["sha256"], Sha256::of(b"# hi\n").as_str());
    let home = tools["sandbox_write_file"]
        .execute(
            call("c2", Some("e2")),
            json!({"file_path": "~/.npmrc", "content": "x=1\n"}),
        )
        .await
        .unwrap();
    assert!(home.ok, "{}", text(&home));
    let ws = world
        .service
        .lookup(&binding.binding)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        world.fake.volume_file(&ws.id, "chats/c1/notes.md"),
        Some(b"# hi\n".to_vec())
    );
    assert_eq!(
        world.fake.home_file(&ws.id, ".npmrc"),
        Some(b"x=1\n".to_vec())
    );
}

#[tokio::test]
async fn a_detached_run_is_refused_at_call_time() {
    let world = World::new(Some(chat_binding(SandboxMode::CHAT)));
    let tools = world.stage().await;
    world.resolver.set(Some(home_binding(SandboxMode::CHAT)));
    let moved = tools["sandbox_run"]
        .execute(call("c1", Some("e1")), json!({"command": ECHO}))
        .await
        .unwrap();
    assert!(not_started(&moved));
    assert_eq!(data(&moved)["code"], "sandbox_not_attached");
    world.resolver.set(None);
    let gone = tools["sandbox_read_file"]
        .execute(call("c2", None), json!({"file_path": "a"}))
        .await
        .unwrap();
    assert_eq!(data(&gone)["errorCode"], "sandbox_not_attached");
    assert_eq!(world.fake.dispatch_calls_total(), 0);
}
