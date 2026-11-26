//! Network host approval: a refused host becomes a pending request; the
//! run's next command or write asks "Allow connections to host:port from
//! this sandbox." for it, and the owner's yes grants exactly the requests
//! that card showed. A host refused after the ask, an answer to another
//! question, or a restart grants nothing.

mod support;

use agentkit_core::Tool;
use agentkit_governance::ToolGuardVerdict;
use agentkit_toolpacks_sandbox::SandboxMode;
use agentkit_workspaces::{NetworkRequest, WorkspaceId};
use serde_json::{Value, json};
use support::{CURL, CURL_OTHER, ECHO, MARK, World, call, home_binding, text};

const CARD: &str = "Allow connections to example.com:443 from this sandbox. (1 attempt)";

fn echo() -> Value {
    json!({"command": ECHO})
}

/// A world whose last command was refused example.com:443.
async fn refused_once() -> (World, WorkspaceId) {
    let binding = home_binding(SandboxMode::CHAT);
    let world = World::new(Some(binding.clone()));
    let tools = world.stage().await;
    let denied = tools["sandbox_run"]
        .execute(call("c1", Some("e1")), json!({"command": CURL}))
        .await
        .unwrap();
    assert!(text(&denied).contains("network_denied"));
    let ws = world
        .service
        .lookup(&binding.binding)
        .await
        .unwrap()
        .unwrap()
        .id;
    (world, ws)
}

async fn pending(world: &World, ws: &WorkspaceId) -> Vec<String> {
    let requests: Vec<NetworkRequest> = world.service.pending_requests(ws).await.unwrap();
    let mut targets: Vec<String> = requests
        .iter()
        .map(|request| request.target.to_string())
        .collect();
    targets.sort();
    targets
}

fn card(verdict: ToolGuardVerdict) -> String {
    match verdict {
        ToolGuardVerdict::AwaitingHuman(ask) => ask.preview.expect("a card"),
        _ => panic!("expected the hosts card"),
    }
}

async fn run(tool: &dyn Tool, call_id: &str, execution: &str, args: Value) -> String {
    text(
        &tool
            .execute(call(call_id, Some(execution)), args)
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn the_answer_grants_exactly_the_requests_the_card_showed() {
    let (world, ws) = refused_once().await;
    let tools = world.stage().await;
    assert_eq!(
        card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await),
        CARD
    );
    // A new host is refused after the card was shown.
    run(
        tools["sandbox_run"].as_ref(),
        "c3",
        "e3",
        json!({"command": CURL_OTHER}),
    )
    .await;
    assert_eq!(
        pending(&world, &ws).await,
        ["example.com:443", "other.example.org:443"]
    );
    // The owner says yes; the resumed attempt re-presents the same card.
    world.grants.approve("c2", &echo());
    let tools = world.stage().await;
    assert_eq!(
        card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await),
        CARD
    );
    let shown = run(tools["sandbox_run"].as_ref(), "c2", "e2", echo()).await;
    assert!(shown.ends_with("[exit code 0]"), "{shown}");
    assert_eq!(
        pending(&world, &ws).await,
        ["other.example.org:443"],
        "only what was shown"
    );
    let now = run(
        tools["sandbox_run"].as_ref(),
        "c4",
        "e4",
        json!({"command": CURL}),
    )
    .await;
    assert!(
        now.ends_with("[exit code 0]"),
        "the granted host connects: {now}"
    );
}

#[tokio::test]
async fn an_answer_to_an_uncertain_rerun_is_not_an_answer_to_the_card() {
    let (world, ws) = refused_once().await;
    let tools = world.stage().await;
    card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await);
    world.grants.approve_rerun("c2", &echo());
    let tools = world.stage().await;
    run(tools["sandbox_run"].as_ref(), "c2", "e2", echo()).await;
    assert_eq!(pending(&world, &ws).await, ["example.com:443"]);
}

#[tokio::test]
async fn a_shown_host_is_asked_again_only_by_the_same_call_shape() {
    let (world, ws) = refused_once().await;
    let tools = world.stage().await;
    card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await);
    // The owner said no (no answer reaches the run); it moves on.
    let tools = world.stage().await;
    assert_eq!(
        card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await),
        CARD
    );
    let other = world
        .verdict(
            tools["sandbox_write_file"].as_ref(),
            json!({"file_path": "a", "content": "x"}),
        )
        .await;
    assert!(
        matches!(other, ToolGuardVerdict::Allowed),
        "not asked twice in one run"
    );
    run(
        tools["sandbox_run"].as_ref(),
        "c5",
        "e5",
        json!({"command": MARK}),
    )
    .await;
    assert_eq!(pending(&world, &ws).await, ["example.com:443"]);
}

#[tokio::test]
async fn after_a_restart_nothing_is_granted() {
    let (mut world, ws) = refused_once().await;
    let tools = world.stage().await;
    card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await);
    world.grants.approve("c2", &echo());
    world.restart_pack();
    let tools = world.stage().await;
    card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await);
    run(tools["sandbox_run"].as_ref(), "c2", "e2", echo()).await;
    assert_eq!(pending(&world, &ws).await, ["example.com:443"]);
}

/// A card raised after the answer existed was spent unseen (a resumed
/// attempt after a restart that ended before its body); the next attempt
/// grants none of its hosts.
#[tokio::test]
async fn a_card_raised_after_the_answer_grants_nothing() {
    let (mut world, ws) = refused_once().await;
    let tools = world.stage().await;
    card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await);
    world.grants.approve("c2", &echo());
    world.restart_pack();
    let tools = world.stage().await;
    run(
        tools["sandbox_run"].as_ref(),
        "c3",
        "e3",
        json!({"command": CURL_OTHER}),
    )
    .await;
    let tools = world.stage().await;
    card(world.verdict(tools["sandbox_run"].as_ref(), echo()).await);
    let tools = world.stage().await;
    run(tools["sandbox_run"].as_ref(), "c2", "e2", echo()).await;
    assert_eq!(
        pending(&world, &ws).await,
        ["example.com:443", "other.example.org:443"]
    );
}
