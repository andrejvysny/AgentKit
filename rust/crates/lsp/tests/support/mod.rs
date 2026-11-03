//! Shared helpers for the LSP integration tests.
//!
//! The module compiles into every LSP test binary; each binary uses only
//! part of the kit, so unused helpers are expected, not dead code.
#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use agentkit_core::{CancellationToken, ToolContext, ToolLimits};
use agentkit_lsp::{LspConfig, ServerCommand};

pub struct ToolHarness;

impl ToolHarness {
    pub fn context(cancellation: CancellationToken) -> ToolContext {
        ToolContext {
            run_id: "test-run".into(),
            tool_call_id: Some("call-1".into()),
            attempt_id: None,
            chat_id: None,
            scope_id: None,
            user_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation,
            dispatch_guard: None,
            metadata: serde_json::Map::new(),
            suspension: None,
        }
    }
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn node_path() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("node");
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Config whose `rust` server is the scripted fake running under node.
/// `tag` is appended to the command line so process-count assertions can
/// isolate one test's fakes from a sibling test's.
pub fn fake_server_config(env: BTreeMap<String, String>, tag: &str) -> LspConfig {
    let node = node_path().expect("node binary (tests/fixtures/fake-server.mjs)");
    let script = fixtures_dir().join("fake-server.mjs");
    let mut config = LspConfig::default();
    config.servers.insert(
        "rust".into(),
        ServerCommand {
            command: node,
            args: vec![
                script.to_string_lossy().into_owned(),
                format!("--tag={tag}"),
            ],
            env,
        },
    );
    config
}

/// True while `/proc/<pid>` exists.
pub fn process_alive(pid: u32) -> bool {
    std::fs::metadata(format!("/proc/{pid}")).is_ok()
}

/// Copy a fixture tree (files + directories, no symlinks needed).
pub fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// A workspace copy that deletes itself when the test finishes.
pub struct FixtureRoot {
    pub path: PathBuf,
}

impl std::ops::Deref for FixtureRoot {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl FixtureRoot {
    pub fn new(fixture: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "agentkit-lsp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        copy_dir(&fixtures_dir().join(fixture), &base).expect("copy fixture");
        Self { path: base }
    }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Count live processes whose command line mentions `needle` (via /proc).
pub fn count_processes_matching(needle: &str) -> usize {
    let mut count = 0;
    let Ok(entries) = fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let text = String::from_utf8_lossy(&cmdline);
        if text.contains(needle) {
            count += 1;
        }
    }
    count
}
