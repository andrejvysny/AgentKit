//! The coding tools as toolsets (T4.1, ADR 0033): `files`, `shell`, `git`
//! and `plan`, each with its card and its versioned guide.

use agentkit_core::Toolset;

pub const FILES_TOOLSET: &str = "files";
pub const SHELL_TOOLSET: &str = "shell";
pub const GIT_TOOLSET: &str = "git";
pub const PLAN_TOOLSET: &str = "plan";

pub fn coding_toolsets() -> Vec<Toolset> {
    vec![
        Toolset::new(
            FILES_TOOLSET,
            "Files",
            "Read, search, list and edit files in the workspace.",
        )
        .with_guide(include_str!("../guides/files.md"))
        .with_tools(&[
            "read_file",
            "list_dir",
            "find_files",
            "code_search",
            "repo_map",
            "edit_file",
            "write_file",
        ]),
        Toolset::new(
            SHELL_TOOLSET,
            "Shell",
            "Run commands and the project's tests in a sandbox.",
        )
        .with_guide(include_str!("../guides/shell.md"))
        .with_tools(&["run_command", "run_tests"]),
        Toolset::new(
            GIT_TOOLSET,
            "Git",
            "Inspect repository status, diffs and history (read-only).",
        )
        .with_guide(include_str!("../guides/git.md"))
        .with_tools(&["git_inspect"]),
        Toolset::new(
            PLAN_TOOLSET,
            "Plan",
            "Submit a written plan and ask for a permission mode.",
        )
        .with_guide(include_str!("../guides/plan.md"))
        .with_tools(&["submit_plan"]),
    ]
}
