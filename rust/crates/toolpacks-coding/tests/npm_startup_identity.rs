#![cfg(unix)]

use agentkit_toolpacks_coding::command_fingerprint;
use std::{collections::HashMap, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn environment(root: &Path) -> HashMap<String, String> {
    HashMap::from([
        ("PATH".into(), std::env::var("PATH").unwrap()),
        ("HOME".into(), root.to_str().unwrap().into()),
        ("NODE_OPTIONS".into(), "--require=./bootstrap.cjs".into()),
        ("npm_config_offline".into(), "true".into()),
    ])
}

fn fixture(root: &Path) {
    std::fs::write(root.join("package.json"), r#"{"name":"startup-identity","version":"1.0.0","scripts":{"echo":"echo unchanged","runner":"startup-runner"}}"#).unwrap();
    std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
    let runner = root.join("node_modules/.bin/startup-runner");
    std::fs::write(
        &runner,
        "#!/usr/bin/env node\nconsole.log('runner unchanged');\n",
    )
    .unwrap();
    std::fs::set_permissions(runner, std::fs::Permissions::from_mode(0o755)).unwrap();
    preload(root, 'a');
}

fn preload(root: &Path, marker: char) {
    std::fs::write(
        root.join("bootstrap.cjs"),
        format!("require('fs').appendFileSync('preload.log', '{marker}');\n"),
    )
    .unwrap();
}

fn execute(
    root: &Path,
    environment: &HashMap<String, String>,
    executable: &str,
    arguments: &[&str],
) {
    let output = Command::new(executable)
        .args(arguments)
        .current_dir(root)
        .env_clear()
        .envs(environment)
        .output()
        .expect("npm/npx must be installed for startup qualification");
    assert!(
        output.status.success(),
        "{executable} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_preload_mutation(root: &Path, executable: &str, arguments: &[&str], command: &str) {
    let environment = environment(root);
    let fingerprint = || {
        command_fingerprint(
            root.to_str().unwrap(),
            root.to_str().unwrap(),
            command,
            &environment,
        )
        .unwrap()
        .expect("fully resolved npm command")
    };
    let approved = fingerprint();
    assert_eq!(fingerprint(), approved, "unchanged inputs retain identity");
    execute(root, &environment, executable, arguments);
    let before = std::fs::read(root.join("preload.log")).unwrap();
    assert!(!before.is_empty());
    assert!(before.iter().all(|byte| *byte == b'a'));
    preload(root, 'b');
    assert_ne!(
        fingerprint(),
        approved,
        "npm preloads NODE_OPTIONS even when its script is echo"
    );
    execute(root, &environment, executable, arguments);
    let after = std::fs::read(root.join("preload.log")).unwrap();
    assert!(after.starts_with(&before));
    assert!(after[before.len()..].iter().all(|byte| *byte == b'b'));
    assert!(
        after.len() > before.len(),
        "mutated preload actually executes"
    );
    preload(root, 'a');
    assert_eq!(fingerprint(), approved, "restored contents retain identity");
}

#[test]
fn actual_npm_startup_preload_mutation_invalidates_echo_and_runner_approvals() {
    for (executable, arguments, command) in [
        ("npm", vec!["run", "echo"], "npm run echo"),
        ("npm", vec!["run", "runner"], "npm run runner"),
        ("npx", vec!["startup-runner"], "npx startup-runner"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fixture(&root);
        assert_preload_mutation(&root, executable, &arguments, command);
    }
}

#[test]
fn npm_launcher_interpreter_replacement_invalidates_identity() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    fixture(&root);
    std::fs::create_dir_all(root.join("bin")).unwrap();
    for (name, contents) in [
        ("npm", "#!/usr/bin/env node\n// unchanged launcher\n"),
        ("node", "#!/bin/sh\nexit 0\n"),
    ] {
        let path = root.join("bin").join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut environment = environment(&root);
    environment.insert(
        "PATH".into(),
        format!("{}:/usr/bin:/bin", root.join("bin").display()),
    );
    let fingerprint = || {
        command_fingerprint(
            root.to_str().unwrap(),
            root.to_str().unwrap(),
            "npm run echo",
            &environment,
        )
        .unwrap()
        .unwrap()
    };
    let approved = fingerprint();
    assert_eq!(fingerprint(), approved);
    std::fs::write(root.join("bin/node"), "#!/bin/sh\nexit 1\n").unwrap();
    assert_ne!(
        fingerprint(),
        approved,
        "unchanged npm launcher selects a changed interpreter"
    );
}

#[test]
fn unresolved_package_manager_startup_inputs_decline_remembered_approval() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    fixture(&root);
    let environment = environment(&root);
    for selector in [
        "--req=./bootstrap.cjs",
        "--im=./bootstrap.cjs",
        "-r./bootstrap.cjs",
        "--loader=./bootstrap.cjs",
    ] {
        let mut changed = environment.clone();
        changed.insert("NODE_OPTIONS".into(), selector.into());
        assert!(
            command_fingerprint(
                root.to_str().unwrap(),
                root.to_str().unwrap(),
                "npm run echo",
                &changed
            )
            .is_err(),
            "unresolved selector {selector}"
        );
    }
    for name in [
        "npm_config_node_options",
        "NPM_CONFIG_NODE_OPTIONS",
        "npm_config_node-options",
        "npm_config_script_shell",
    ] {
        let mut changed = environment.clone();
        changed.insert(name.into(), "./bootstrap.cjs".into());
        assert!(
            command_fingerprint(
                root.to_str().unwrap(),
                root.to_str().unwrap(),
                "npm run echo",
                &changed
            )
            .is_err(),
            "unresolved environment {name}"
        );
    }
    std::fs::write(
        root.join(".npmrc"),
        "node-options=--require=./bootstrap.cjs\n",
    )
    .unwrap();
    assert!(
        command_fingerprint(
            root.to_str().unwrap(),
            root.to_str().unwrap(),
            "npm run echo",
            &environment
        )
        .is_err(),
        "config-selected startup files require fresh approval"
    );
}

#[test]
fn relative_and_quoted_npm_configuration_cannot_hide_startup_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    fixture(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();
    let mut environment = environment(&root);
    environment.insert("NPM_CONFIG_USERCONFIG".into(), "config/custom.ini".into());
    for text in [
        "node-options=--require=./bootstrap.cjs",
        "\"node-options\"=--require=./bootstrap.cjs",
        "node-options[]=--require=./bootstrap.cjs",
    ] {
        std::fs::write(root.join("config/custom.ini"), text).unwrap();
        assert!(
            command_fingerprint(
                root.to_str().unwrap(),
                root.to_str().unwrap(),
                "npm run echo",
                &environment
            )
            .is_err(),
            "unresolved selected config must decline: {text}"
        );
    }
    std::fs::write(
        root.join("config/custom.ini"),
        "registry=https://registry.npmjs.org\n",
    )
    .unwrap();
    assert!(
        command_fingerprint(
            root.to_str().unwrap(),
            root.to_str().unwrap(),
            "npm run echo",
            &environment
        )
        .unwrap()
        .is_some()
    );
}
