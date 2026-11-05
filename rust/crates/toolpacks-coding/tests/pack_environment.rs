//! The pack's `environment` option and the standing-approval key must
//! fingerprint the same map, or a grant would authorise a different spawn
//! environment than the one the command runs under.

use std::collections::HashMap;

use agentkit_toolpacks_coding::command_policy::StandingApprovalWorkspace;

use agentkit_toolpacks_coding::{
    CODING_NAMESPACE, CodingToolPackOptions, CodingWorkspace, SandboxMode, ShellArgs,
    coding_standing_approval_key, create_coding_tool_pack,
};

async fn workspace_with_stub(tag: &str) -> (String, String) {
    let root = std::env::temp_dir().join(format!("ak-packenv-{tag}-{}", std::process::id()));
    let bin = root.join("bin");
    tokio::fs::create_dir_all(&bin).await.unwrap();
    let stub = bin.join("bun");
    tokio::fs::write(&stub, "#!/bin/sh\nexit 0\n")
        .await
        .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
    }
    let canon = |p: std::path::PathBuf| async move {
        tokio::fs::canonicalize(p)
            .await
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    (canon(root).await, canon(bin).await)
}

async fn key_digest(root: &str, environment: HashMap<String, String>) -> String {
    coding_standing_approval_key(
        CODING_NAMESPACE,
        Some("chat-1"),
        "run_command",
        &ShellArgs {
            command: "bun test".into(),
            cwd: None,
        },
        &StandingApprovalWorkspace {
            root: root.to_owned(),
            identity: "test".into(),
            context: None,
            environment: Some(environment),
        },
    )
    .await
    .expect("keyable")
    .digest
}

#[tokio::test]
async fn pack_environment_and_approval_key_fingerprint_the_same_map() {
    let (root, bin) = workspace_with_stub("agree").await;
    let environment = HashMap::from([("PATH".to_owned(), bin)]);
    let pack = create_coding_tool_pack(CodingToolPackOptions {
        workspace: CodingWorkspace { root: root.clone() },
        sandbox: SandboxMode::Off,
        environment: Some(environment.clone()),
        ..CodingToolPackOptions::default()
    })
    .await
    .unwrap();
    let spawned = pack
        .env()
        .environment
        .as_deref()
        .cloned()
        .expect("pack carries the explicit environment");
    assert_eq!(spawned, environment);
    let spawned_copy = spawned.clone();
    assert_eq!(
        key_digest(&root, spawned).await,
        key_digest(&root, environment.clone()).await,
        "the spawn's map and the keyed map must agree"
    );

    let mut different = environment;
    different.insert(
        "PATH".to_owned(),
        format!("{root}/nowhere:{}", different["PATH"]),
    );
    assert_ne!(
        key_digest(&root, different).await,
        key_digest(&root, spawned_copy).await,
        "a different environment must yield a different key"
    );
    let _ = tokio::fs::remove_dir_all(&root).await;
}
