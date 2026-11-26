//! SBX-17 and the mode table: every mode against every tool through the
//! guard, the same refusals from the bodies when no guard is registered,
//! and the host authority's live mode winning over the pinned one.

mod support;

use std::sync::Arc;

use agentkit_core::permission::CodingMode;
use agentkit_governance::{
    CodingPermissionAuthority, GovernanceError, GovernanceResult, PermissionContext, ToolGuard,
    ToolGuardContext, ToolGuardVerdict,
};
use agentkit_toolpacks_sandbox::{
    SandboxEnvironment, SandboxMode, SandboxPackOptions, SandboxToolPack,
};
use serde_json::{Value, json};
use support::{World, call, chat_binding, data, not_started};

const MODES: [SandboxMode; 5] = [
    SandboxMode::ReadOnly,
    SandboxMode::Plan,
    SandboxMode::AskBeforeChanges,
    SandboxMode::EditAutomatically,
    SandboxMode::FullAccess,
];

fn args(tool: &str) -> Value {
    match tool {
        "sandbox_run" => json!({"command": "echo hi"}),
        "sandbox_write_file" => json!({"file_path": "a.txt", "content": "x"}),
        "sandbox_read_file" | "sandbox_export_file" => json!({"file_path": "report.txt"}),
        _ => json!({}),
    }
}

fn shape(verdict: &ToolGuardVerdict) -> &'static str {
    match verdict {
        ToolGuardVerdict::Allowed => "allow",
        ToolGuardVerdict::Refused(_) => "refuse",
        ToolGuardVerdict::AwaitingHuman(_) => "ask",
    }
}

fn expected(mode: SandboxMode, tool: &str) -> &'static str {
    let changes = matches!(
        tool,
        "sandbox_run" | "sandbox_write_file" | "sandbox_export_file"
    );
    match mode {
        _ if !changes => "allow",
        SandboxMode::ReadOnly | SandboxMode::Plan => "refuse",
        SandboxMode::FullAccess => "allow",
        _ if tool == "sandbox_export_file" => "ask",
        _ => "allow",
    }
}

/// SBX-17: the whole table, five modes by six tools.
#[tokio::test]
async fn every_mode_judges_every_tool_as_the_table_says() {
    let world = World::new(Some(chat_binding(SandboxMode::CHAT)));
    world
        .put(&chat_binding(SandboxMode::CHAT), "report.txt", "total 42\n")
        .await;
    let tools = world.stage().await;
    for mode in MODES {
        world.resolver.set(Some(chat_binding(mode)));
        for (name, tool) in &tools {
            let verdict = world.verdict(tool.as_ref(), args(name)).await;
            assert_eq!(shape(&verdict), expected(mode, name), "{mode:?} {name}");
            if let ToolGuardVerdict::Refused(reason) = &verdict {
                let code = if mode == SandboxMode::Plan {
                    "plan_mode:"
                } else {
                    "read_only:"
                };
                assert!(reason.starts_with(code), "{reason}");
            }
        }
    }
}

/// Without the guard, the bodies refuse the same calls: nothing runs,
/// nothing is written, nothing leaves.
#[tokio::test]
async fn read_only_and_plan_bodies_change_nothing_without_the_guard() {
    for (mode, code) in [
        (SandboxMode::ReadOnly, "read_only"),
        (SandboxMode::Plan, "plan_mode"),
    ] {
        let world = World::new(Some(chat_binding(mode)));
        let tools = world.stage().await;
        for (n, tool) in ["sandbox_run", "sandbox_write_file"]
            .into_iter()
            .enumerate()
        {
            let result = tools[tool]
                .execute(
                    call(&format!("c{n}"), Some(&format!("exec-{n}"))),
                    args(tool),
                )
                .await
                .unwrap();
            assert!(not_started(&result), "{mode:?} {tool}");
            assert_eq!(data(&result)["code"], code);
        }
        let export = tools["sandbox_export_file"]
            .execute(call("c9", Some("exec-9")), args("sandbox_export_file"))
            .await
            .unwrap();
        assert_eq!(data(&export)["errorCode"], code);
        assert_eq!(world.fake.dispatch_calls_total(), 0);
        assert_eq!(world.sink.count(), 0);
    }
}

struct Authority(Option<CodingMode>, bool);

#[async_trait::async_trait]
impl CodingPermissionAuthority for Authority {
    async fn mode_of(&self, _: &PermissionContext) -> GovernanceResult<Option<CodingMode>> {
        if self.1 {
            return Err(GovernanceError::Port("mode store down".into()));
        }
        Ok(self.0)
    }
}

fn pack_with(world: &World, authority: Authority) -> SandboxToolPack {
    SandboxToolPack::new(SandboxPackOptions {
        service: world.service.clone(),
        resolver: world.resolver.clone(),
        export_sink: world.sink.clone(),
        authority: Some(Arc::new(authority)),
        environment: SandboxEnvironment::new("test"),
    })
}

/// The session's live mode beats the pinned one; an authority that cannot
/// answer fails the call closed.
#[tokio::test]
async fn the_host_authority_overrides_the_pinned_mode_and_fails_closed() {
    let world = World::new(Some(chat_binding(SandboxMode::FullAccess)));
    let tools = world.stage().await;
    let run = tools["sandbox_run"].definition().clone();
    let context = ToolGuardContext {
        contribution: world.contribution(),
        namespace: "sandbox".into(),
        tool: run,
        arguments: Some(args("sandbox_run")),
    };
    let read_only = pack_with(&world, Authority(Some(CodingMode::ReadOnly), false));
    let verdict = read_only
        .permission_guard()
        .can_execute(&context)
        .await
        .unwrap();
    assert_eq!(shape(&verdict), "refuse");
    let silent = pack_with(&world, Authority(None, false));
    let verdict = silent
        .permission_guard()
        .can_execute(&context)
        .await
        .unwrap();
    assert_eq!(
        shape(&verdict),
        "allow",
        "no opinion: the pinned mode stands"
    );
    let broken = pack_with(&world, Authority(None, true));
    assert!(
        broken
            .permission_guard()
            .can_execute(&context)
            .await
            .is_err()
    );
    use agentkit_governance::ToolContributor;
    let staged = read_only.contribute(&world.contribution()).await.unwrap();
    let agentkit_governance::ContributedTool::DirectWrite(run) = &staged[0] else {
        panic!("sandbox_run is a direct write");
    };
    let result = run
        .execute(call("c1", Some("exec-1")), args("sandbox_run"))
        .await
        .unwrap();
    assert!(not_started(&result));
    assert_eq!(world.fake.dispatch_calls_total(), 0);
}
