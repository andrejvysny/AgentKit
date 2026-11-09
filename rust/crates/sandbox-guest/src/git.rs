//! Read-only git inspection (§8.4). Fixed argv, no pager/hooks/ext-helpers,
//! minimal env, bounded output. The `rev` from the wire is revalidated here
//! — the host's check is never trusted.

use std::collections::HashMap;
use std::time::Duration;

use agentkit_sandbox_contracts::api::{GitCommand, GitInspectQuery, GitInspectReply};
use agentkit_sandbox_contracts::limits::MAX_GIT_OUTPUT_BYTES;
use tokio::process::Command;

use crate::error::{GuestFail, code};
use crate::fsops::GuestFs;

/// The exact environment git runs with — nothing else, ever.
fn git_env() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/agent"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("LANG", "C"),
    ])
}

/// True when `git` exists under the fixed PATH (capability advertisement).
#[must_use]
pub fn git_available() -> bool {
    crate::handshake::binary_exists("git")
}

pub async fn git_inspect(
    fs: &GuestFs,
    query: &GitInspectQuery,
) -> Result<GitInspectReply, GuestFail> {
    if !git_available() {
        return Err(GuestFail::new(code::unsupported(), "git not installed"));
    }
    // Resolve the repo dir through the verified root mapping (never raw wire
    // paths): open_dir verifies S_ISDIR beneath the root, then we use the
    // derived on-disk path.
    let repo_path = fs.real_path(&query.repo)?;

    if let Some(rev) = &query.rev
        && !agentkit_sandbox_contracts::api::validate_git_rev(rev)
    {
        return Err(GuestFail::new(code::BAD_INPUT, "invalid rev"));
    }
    for f in &query.path_filters {
        validate_path_filter(f)?;
    }

    let repo_str = repo_path.to_string_lossy().to_string();
    let mut argv: Vec<String> = vec![
        "git".into(),
        "-C".into(),
        repo_str.clone(),
        "--no-pager".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
    ];
    let diff_flavor = matches!(query.command, GitCommand::Diff);
    if diff_flavor {
        argv.push("--no-ext-diff".into());
        argv.push("--no-textconv".into());
    }
    let max_count = query.max_count.min(1000).to_string();
    match query.command {
        GitCommand::Status => argv.extend(["status".into(), "--porcelain=v1".into()]),
        GitCommand::Diff => {
            argv.push("diff".into());
            argv.push("--no-color".into());
        }
        GitCommand::Log => {
            argv.push("log".into());
            argv.push("--no-color".into());
            argv.push("--format=%H %ct %s".into());
            argv.push("-n".into());
            argv.push(max_count.clone());
            if let Some(rev) = &query.rev {
                argv.push(rev.clone());
            } else {
                argv.push("HEAD".into());
            }
        }
        GitCommand::Show => {
            argv.push("show".into());
            argv.push("--no-color".into());
            argv.push(query.rev.clone().unwrap_or_else(|| "HEAD".into()));
        }
        GitCommand::RevParse => {
            argv.push("rev-parse".into());
            argv.push(query.rev.clone().unwrap_or_else(|| "HEAD".into()));
        }
    }
    if !query.path_filters.is_empty() {
        argv.push("--".into());
        argv.extend(query.path_filters.iter().cloned());
    }

    let mut cmd = Command::new(if std::path::Path::new("/usr/bin/git").exists() {
        "/usr/bin/git"
    } else {
        "/bin/git"
    });
    cmd.args(&argv[1..])
        .env_clear()
        .envs(git_env())
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());

    let timeout = Duration::from_secs(30);
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(GuestFail::io("git spawn", &e)),
        Err(_) => return Err(GuestFail::new(code::TIMEOUT, "git timed out")),
    };

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    let truncated = text.len() > MAX_GIT_OUTPUT_BYTES;
    if truncated {
        let mut cut = MAX_GIT_OUTPUT_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Ok(GitInspectReply {
        sha256: agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(text.as_bytes()),
        text,
        truncated,
        exit_code: output.status.code().unwrap_or(-1),
    })
}

fn validate_path_filter(f: &str) -> Result<(), GuestFail> {
    if f.is_empty()
        || f.len() > 512
        || f.starts_with('-')
        || f.starts_with('/')
        || f.contains('\0')
        || f.split('/').any(|s| s == "." || s == ".." || s.is_empty())
        || f.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
    {
        return Err(GuestFail::new(code::BAD_INPUT, "invalid path filter"));
    }
    Ok(())
}
