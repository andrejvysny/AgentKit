//! The common dialect, version 1: the coding-tool shape shared by Claude
//! Code, Qwen's CLI and Gemini's CLI, and the fallback for any model.
//!
//! The canonical schemas already carry the common parameter names (T1.3), so
//! this dialect renames tools and splits `git_inspect`; it renames no
//! parameter. Tools not listed here pass through unchanged.

use super::{Dialect, ToolForm, WireTool};
use serde_json::json;

pub const COMMON_ID: &str = "common";
pub const COMMON_VERSION: &str = "1";

const GIT_SAFETY: &str = "Repository hooks, fsmonitor, filter, textconv and external-diff drivers, the pager and ssh helpers are neutralized and never run; paths are taken literally; output is capped.";

fn same(name: &str) -> ToolForm {
    renamed(name, name)
}

fn renamed(canonical: &str, wire: &str) -> ToolForm {
    ToolForm {
        canonical: canonical.to_owned(),
        wires: vec![WireTool {
            name: wire.to_owned(),
            params: None,
            fixed: vec![],
            description: None,
        }],
    }
}

fn git(wire: &str, command: &str, params: &[&str], description: String) -> WireTool {
    WireTool {
        name: wire.to_owned(),
        params: Some(params.iter().map(|param| (*param).to_owned()).collect()),
        fixed: vec![("command".to_owned(), json!(command))],
        description: Some(description),
    }
}

/// `git_inspect` becomes four flat tools; each translates back to
/// `git_inspect` with its `command`.
fn git_forms() -> ToolForm {
    ToolForm {
        canonical: "git_inspect".to_owned(),
        wires: vec![
            git(
                "git_status",
                "status",
                &["path"],
                format!("Show the working tree status and the current branch (read-only). Use it instead of running git through run_shell_command. path limits the report to one file or directory. {GIT_SAFETY}"),
            ),
            git(
                "git_diff",
                "diff",
                &["path", "staged", "rev"],
                format!("Show changes (read-only): the unstaged working tree changes, or the staged changes with staged true; rev compares against a commit, branch, tag or range, and path limits the diff to one file or directory. Use it instead of running git through run_shell_command. {GIT_SAFETY}"),
            ),
            git(
                "git_log",
                "log",
                &["path", "limit", "rev"],
                format!("List commits, newest first (read-only). limit sets how many (default 20); rev starts from a commit, branch or tag; path keeps only commits that touch it. Use it instead of running git through run_shell_command. {GIT_SAFETY}"),
            ),
            git(
                "git_show",
                "show",
                &["path", "rev"],
                format!("Show one commit with its changes (read-only). rev names the commit, branch or tag (default HEAD), for example HEAD~2; path limits the output to one file or directory. Use it instead of running git through run_shell_command. {GIT_SAFETY}"),
            ),
        ],
    }
}

/// Common v1. The table is the spec's "common shape".
pub fn common() -> Dialect {
    Dialect {
        id: COMMON_ID.to_owned(),
        version: COMMON_VERSION.to_owned(),
        tools: vec![
            same("read_file"),
            same("write_file"),
            renamed("edit_file", "edit"),
            renamed("list_dir", "list_directory"),
            renamed("find_files", "glob"),
            renamed("code_search", "grep_search"),
            renamed("run_command", "run_shell_command"),
            git_forms(),
            same("run_tests"),
            same("repo_map"),
            same("submit_plan"),
        ],
    }
}
