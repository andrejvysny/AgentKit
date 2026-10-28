//! The three read-only search tools. Port of
//! `packages/toolpacks-coding/src/search-tools.ts` onto the engine in
//! `crate::search`.

use std::sync::Arc;

use agentkit_core::{Tool, ToolContext};
use serde_json::{Value, json};

use crate::args::{as_record, opt_bool, opt_int, opt_string, req_string};
use crate::context::{Outcome, PackEnv, ToolEffect, ToolSpec, define_tool};
use crate::errors::CodingError;
use crate::paths::{ConfineOptions, PathAccess, confine};
use crate::search::{
    SearchInput, WalkOptions, find_files as engine_find_files, repo_map as engine_repo_map,
    search_text,
};

const PREFER_NATIVE: &str = "Use this instead of grep, rg, find or ls in a shell command.";

/// JS number rendering inside interpolated descriptions (integral floats print
/// without a fraction).
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `walkBase(env, signal)` from the TS wrappers.
fn walk_base(env: &PackEnv, ctx: &ToolContext) -> WalkOptions {
    WalkOptions::new(
        env.limits.clone(),
        env.allow_sensitive,
        Arc::clone(&env.clock),
        Some(ctx.cancellation.clone()),
    )
}

pub fn find_files_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let description = format!(
        "Find files by glob (e.g. **/*.ts, src/**/*.test.{{ts,tsx}}). Respects .gitignore, .ignore and .git/info/exclude; skips .git, node_modules, symlinks and credential files. Up to {} results. {PREFER_NATIVE}",
        env.limits.find_max_results
    );
    let input_schema = json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob matched against the path relative to `path`, or the file name."
            },
            "path": {
                "type": "string",
                "description": "Directory to search. Default: the workspace root."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": env.limits.find_max_results
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    });
    let env = Arc::clone(env);
    define_tool(
        ToolSpec {
            name: "find_files",
            effect: ToolEffect::Read,
            capability: "fs.read",
            description,
            input_schema,
            timeout_ms: env.limits.search_time_budget_ms + 5000,
        },
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            async move {
                let input = as_record(&raw)?;
                let pattern = req_string(input, "pattern", env.limits.search_max_pattern_chars)?;
                let path = opt_string(input, "path", 4096)?.unwrap_or_else(|| ".".to_owned());
                let limit = opt_int(input, "limit", 1, env.limits.find_max_results as i64)?
                    .unwrap_or(env.limits.find_default_results as i64);
                let root = env.root().await?;
                let target = confine(
                    &root,
                    &path,
                    ConfineOptions {
                        access: PathAccess::Read,
                        allow_sensitive: env.allow_sensitive,
                    },
                )
                .await?;
                let info = tokio::fs::metadata(&target.path).await.ok();
                if !info.is_some_and(|info| info.is_dir()) {
                    return Err(CodingError::new(
                        "not_a_directory",
                        format!("{path} is not a directory"),
                    ));
                }
                let found = engine_find_files(
                    &pattern,
                    &target.path,
                    &root,
                    limit as usize,
                    &walk_base(&env, &ctx),
                )
                .await?;
                let body = if found.paths.is_empty() {
                    "No files matched".to_owned()
                } else {
                    found.paths.join("\n")
                };
                Ok(Outcome {
                    ok: true,
                    text: if found.stopped.is_empty() {
                        body
                    } else {
                        format!("{body}\n[stopped: {}]", found.stopped)
                    },
                    summary: Some(format!("{} file(s) matched {pattern}", found.paths.len())),
                    truncated: !found.stopped.is_empty(),
                    warnings: vec![],
                    details: None,
                })
            }
        },
    )
}

pub fn code_search_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let l = &env.limits;
    let description = format!(
        "Search text files for a regex or literal. Output lines are \"<path>:<line>:<text>\". Respects .gitignore, .ignore and .git/info/exclude; skips .git, node_modules, symlinks, binary and credential files. Bounded: {} entries, {} MiB per file, {} KiB output, {}s; a bounded search says so. {PREFER_NATIVE}",
        l.search_max_files,
        js_number(l.search_max_file_bytes as f64 / 1024.0 / 1024.0),
        js_number(l.search_max_output_bytes as f64 / 1024.0),
        js_number(l.search_time_budget_ms as f64 / 1000.0)
    );
    let input_schema = json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regular expression (or literal text with literal=true)."
            },
            "path": {
                "type": "string",
                "description": "File or directory. Default: the workspace root."
            },
            "glob": {
                "type": "string",
                "description": "Only files whose path or name matches, e.g. *.ts."
            },
            "literal": {
                "type": "boolean",
                "description": "Treat pattern as plain text."
            },
            "ignoreCase": { "type": "boolean" },
            "context": {
                "type": "integer",
                "minimum": 0,
                "maximum": l.search_max_context,
                "description": "Lines of context around each match."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": l.search_max_matches,
                "description": format!("Maximum matches (default {}).", l.search_default_matches)
            },
            "offset": {
                "type": "integer",
                "minimum": 0,
                "description": "Matches to skip, to page through a truncated result (use the offset the notice gives)."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    });
    let timeout_ms = l.search_time_budget_ms + 5000;
    let limits = env.limits.clone();
    let env = Arc::clone(env);
    define_tool(
        ToolSpec {
            name: "code_search",
            effect: ToolEffect::Read,
            capability: "fs.read",
            description,
            input_schema,
            timeout_ms,
        },
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            let limits = limits.clone();
            async move {
                let input = as_record(&raw)?;
                let pattern = req_string(input, "pattern", limits.search_max_pattern_chars)?;
                let path = opt_string(input, "path", 4096)?.unwrap_or_else(|| ".".to_owned());
                let root = env.root().await?;
                let target = confine(
                    &root,
                    &path,
                    ConfineOptions {
                        access: PathAccess::Read,
                        allow_sensitive: env.allow_sensitive,
                    },
                )
                .await?;
                let search = SearchInput {
                    target: target.path,
                    workspace_root: root,
                    pattern: pattern.clone(),
                    glob: opt_string(input, "glob", 1000)?,
                    literal: opt_bool(input, "literal")?.unwrap_or(false),
                    ignore_case: opt_bool(input, "ignoreCase")?.unwrap_or(false),
                    context: opt_int(input, "context", 0, limits.search_max_context as i64)?
                        .unwrap_or(0) as usize,
                    limit: opt_int(input, "limit", 1, limits.search_max_matches as i64)?
                        .unwrap_or(limits.search_default_matches as i64)
                        as usize,
                    offset: opt_int(input, "offset", 0, 1_000_000_000)?.unwrap_or(0) as usize,
                };
                let outcome = search_text(&search, &walk_base(&env, &ctx)).await?;
                Ok(Outcome {
                    ok: true,
                    text: outcome.text,
                    summary: Some(format!(
                        "{} match(es) for {}",
                        outcome.matches,
                        pattern.chars().take(60).collect::<String>()
                    )),
                    truncated: outcome.truncated,
                    warnings: vec![],
                    details: None,
                })
            }
        },
    )
}

pub fn repo_map_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let l = &env.limits;
    let description = format!(
        "Orient yourself in a repository: top-level manifests (package.json, tsconfig, Cargo.toml, pyproject, go.mod, README) with short excerpts, plus a depth-limited directory tree (default depth {}, max {} entries). Respects ignore files. Call this first, before any shell command.",
        l.repo_map_default_depth, l.repo_map_max_entries
    );
    let input_schema = json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Directory. Default: the workspace root."
            },
            "depth": { "type": "integer", "minimum": 1, "maximum": l.repo_map_max_depth }
        },
        "additionalProperties": false
    });
    let timeout_ms = l.search_time_budget_ms + 5000;
    let env = Arc::clone(env);
    define_tool(
        ToolSpec {
            name: "repo_map",
            effect: ToolEffect::Read,
            capability: "fs.read",
            description,
            input_schema,
            timeout_ms,
        },
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            async move {
                let input = as_record(&raw)?;
                let path = opt_string(input, "path", 4096)?.unwrap_or_else(|| ".".to_owned());
                let depth = opt_int(input, "depth", 1, env.limits.repo_map_max_depth as i64)?
                    .unwrap_or(env.limits.repo_map_default_depth as i64);
                let root = env.root().await?;
                let target = confine(
                    &root,
                    &path,
                    ConfineOptions {
                        access: PathAccess::Read,
                        allow_sensitive: env.allow_sensitive,
                    },
                )
                .await?;
                let info = tokio::fs::metadata(&target.path).await.ok();
                if !info.is_some_and(|info| info.is_dir()) {
                    return Err(CodingError::new(
                        "not_a_directory",
                        format!("{path} is not a directory"),
                    ));
                }
                let map =
                    engine_repo_map(&target.path, &root, depth as usize, &walk_base(&env, &ctx))
                        .await?;
                Ok(Outcome {
                    ok: true,
                    text: map.text,
                    summary: Some(format!(
                        "mapped {} ({} entries)",
                        if target.relative_path.is_empty() {
                            "."
                        } else {
                            &target.relative_path
                        },
                        map.entries
                    )),
                    truncated: map.truncated,
                    warnings: vec![],
                    details: None,
                })
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::default_in_process_coordinator;
    use crate::limits::CodingLimits;
    use crate::types::{SandboxMode, system_clock};
    use agentkit_core::{ToolLimits, ToolResult};
    use serde_json::json;
    use std::path::PathBuf;

    struct TempRoot {
        root: PathBuf,
    }

    impl TempRoot {
        async fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("ak-search-tools-{tag}-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let root = tokio::fs::canonicalize(&root).await.unwrap();
            Self { root }
        }

        async fn put(&self, rel: &str, content: &str) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.unwrap();
            }
            tokio::fs::write(&path, content).await.unwrap();
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn env_for(root: &std::path::Path) -> Arc<PackEnv> {
        Arc::new(PackEnv::new(crate::context::EnvInit {
            workspace_root: root.to_string_lossy().into_owned(),
            limits: CodingLimits::default(),
            allow_sensitive: false,
            clock: system_clock(),
            sandbox: SandboxMode::Off,
            network: false,
            allow_git_writes: false,
            test_commands: vec![],
            platform: "linux".to_owned(),
            sandbox_exec_path: String::new(),
            coordinator: default_in_process_coordinator(),
            coordination_wait_ms: 0,
        }))
    }

    fn context() -> ToolContext {
        ToolContext {
            run_id: "test-run".to_owned(),
            chat_id: None,
            scope_id: None,
            user_id: None,
            bindings: vec![],
            limits: ToolLimits::default(),
            cancellation: agentkit_core::CancellationToken::new(),
            dispatch_guard: None,
            metadata: serde_json::Map::new(),
        }
    }

    fn text(result: &ToolResult) -> String {
        match &result.model_data {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        }
    }

    async fn run(tool: &Arc<dyn Tool>, input: Value) -> ToolResult {
        tool.execute(context(), input).await.unwrap()
    }

    #[tokio::test]
    async fn find_files_matches_globs_and_reports_limit() {
        let h = TempRoot::new("find").await;
        h.put(".gitignore", "dist/\n").await;
        h.put("src/a.ts", "").await;
        h.put("dist/d.ts", "").await;
        let tool = find_files_tool(&env_for(&h.root));
        let result = run(&tool, json!({ "pattern": "**/*.ts" })).await;
        assert!(result.ok);
        assert_eq!(text(&result), "src/a.ts");
        assert_eq!(result.summary.as_deref(), Some("1 file(s) matched **/*.ts"));
        assert!(!result.truncated);

        let one = run(&tool, json!({ "pattern": "**/*", "limit": 1 })).await;
        assert!(text(&one).contains("[stopped: result limit 1 reached]"));
        assert!(one.truncated);

        let none = run(&tool, json!({ "pattern": "*.md" })).await;
        assert_eq!(text(&none), "No files matched");
    }

    #[tokio::test]
    async fn find_files_rejects_non_directory_and_bad_arguments() {
        let h = TempRoot::new("findbad").await;
        h.put("f.txt", "").await;
        let tool = find_files_tool(&env_for(&h.root));
        let result = run(&tool, json!({ "pattern": "*", "path": "f.txt" })).await;
        assert!(!result.ok);
        assert!(text(&result).contains("not_a_directory"));
        assert!(text(&result).contains("f.txt is not a directory"));

        let pattern = "x".repeat(5000);
        let result = run(&tool, json!({ "pattern": pattern })).await;
        assert!(!result.ok);
        assert!(text(&result).contains("invalid_arguments"));

        let result = run(&tool, json!({})).await;
        assert!(!result.ok);
        assert!(text(&result).contains("\"pattern\" must be a non-empty string"));
    }

    #[tokio::test]
    async fn every_search_tool_caps_the_path_argument_at_4096() {
        // TS `optString` defaults to maxLength 4096 at every `path` site.
        let h = TempRoot::new("pathcap").await;
        let env = env_for(&h.root);
        let long_path = "x".repeat(5000);
        for tool in [
            find_files_tool(&env),
            code_search_tool(&env),
            repo_map_tool(&env),
        ] {
            let result = run(&tool, json!({ "pattern": "x", "path": long_path })).await;
            assert!(!result.ok, "{} must reject an oversized path", tool.definition().name);
            assert!(
                text(&result).contains("\"path\" is longer than 4096 characters"),
                "{}: {}",
                tool.definition().name,
                text(&result)
            );
        }
    }

    #[tokio::test]
    async fn code_search_summary_and_prefer_native_blurb() {
        let h = TempRoot::new("search").await;
        h.put("a.txt", "hit 1\nmiss\nhit 2\n").await;
        let env = env_for(&h.root);
        assert!(env.limits.search_max_file_bytes > 0);
        let tool = code_search_tool(&env);
        let definition = tool.definition();
        assert_eq!(definition.name, "code_search");
        assert_eq!(definition.effect, "read");
        assert_eq!(definition.capability, "fs.read");
        assert_eq!(definition.timeout_ms, Some(25_000));
        assert!(definition.description.contains("instead of grep"));

        let result = run(&tool, json!({ "pattern": "hit" })).await;
        assert!(result.ok);
        assert_eq!(result.summary.as_deref(), Some("2 match(es) for hit"));
        assert_eq!(text(&result), "a.txt:1:hit 1\na.txt:3:hit 2");
    }

    #[tokio::test]
    async fn code_search_rejects_sensitive_direct_path_outside_and_bad_numbers() {
        let h = TempRoot::new("searchbad").await;
        h.put(".env", "API=needle\n").await;
        let tool = code_search_tool(&env_for(&h.root));

        let result = run(&tool, json!({ "pattern": "needle", "path": ".env" })).await;
        assert!(!result.ok);
        assert!(text(&result).contains("path_sensitive"));

        let result = run(&tool, json!({ "pattern": "x", "path": ".." })).await;
        assert!(!result.ok);
        assert!(text(&result).contains("path_outside_root"));

        let result = run(&tool, json!({ "pattern": "hit", "limit": 5000 })).await;
        assert!(!result.ok);
        assert!(text(&result).contains("invalid_arguments"));

        let result = run(&tool, json!({ "pattern": "hit", "context": 21 })).await;
        assert!(!result.ok);
        assert!(text(&result).contains("invalid_arguments"));

        let result = run(&tool, json!({ "pattern": "(" })).await;
        assert!(!result.ok);
        assert!(text(&result).contains("invalid_pattern"));
    }

    #[tokio::test]
    async fn repo_map_shows_tree_and_summary() {
        let h = TempRoot::new("map").await;
        h.put("package.json", "{\"name\":\"demo\"}").await;
        h.put("src/index.ts", "").await;
        let tool = repo_map_tool(&env_for(&h.root));

        let result = run(&tool, json!({})).await;
        assert!(result.ok);
        let body = text(&result);
        assert!(body.contains("== package.json ==\n{\"name\":\"demo\"}"));
        assert!(body.contains("== tree (depth 3) =="));
        assert_eq!(result.summary.as_deref(), Some("mapped . (3 entries)"));

        let deep = run(&tool, json!({ "depth": 0 })).await;
        assert!(!deep.ok);
        assert!(text(&deep).contains("invalid_arguments"));
    }
}
