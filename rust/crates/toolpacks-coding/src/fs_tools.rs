//! The file tools: `read_file`, `list_dir`, `write_file`, `edit_file`.
//! Port of `packages/toolpacks-coding/src/fs-tools.ts`.
//!
//! Every user-visible failure mirrors the TS `code: message` text exactly.
//! Mutations take a file lease from `env.coordinator` (waiting at most
//! `env.coordination_wait_ms`), re-resolve their path under the lease, and
//! verify the bytes one last time before the atomic rename.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use agentkit_core::{Tool, ToolContext};

use crate::args::{as_record, opt_bool, opt_int, opt_string, req_string};
use crate::context::{Outcome, PackEnv, ToolEffect, ToolSpec, define_tool};
use crate::coordination::{AcquireFileInput, LeaseHolder};
use crate::diff::unified_diff;
use crate::edit_engine::{Edit, apply_edits, split_bom};
use crate::errors::{CodingError, file_changed_error, file_not_read_error};
use crate::fs_util::{
    atomic_write, create_if_absent, decode_utf8, looks_binary, read_regular_file, sha256,
};
use crate::paths::{ConfineOptions, ConfinedPath, PathAccess, confine, is_sensitive_segment};
use crate::search::load_ignore_matcher;
use crate::truncate::{TruncatedBy, TruncationLimits, format_size, split_lines, truncate_head};
use crate::version_ledger as ledger;

const NATIVE_HINT: &str = "Prefer the native tools (read_file, list_dir, find_files, code_search, repo_map) over shell commands for exploring code.";

const MAX_READ_PATHS: usize = 8;

/// JS `holderOf`: the label is `slice(0, 200)` over UTF-16 code units.
fn slice_utf16(value: &str, max_units: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().take(max_units).collect();
    String::from_utf16_lossy(&units)
}

fn holder_of(ctx: &ToolContext, tool: &str, label: &str) -> LeaseHolder {
    LeaseHolder {
        chat_id: ctx.chat_id.clone(),
        run_id: ctx.run_id.clone(),
        // `ToolContext` carries no toolCallId; the record keeps it unset.
        tool_call_id: None,
        tool: tool.to_owned(),
        label: slice_utf16(label, 200),
    }
}

/// Stable identity of one mutating file operation: the same
/// `{owner_pid}:{token}` grammar as the command's `AGENTKIT_EXEC_ID` marker,
/// preferring the tool call's stable identity so every attempt of one logical
/// call names one effect (ADR 0020); a missing or whitespace-bearing id falls
/// back to a minted uuid. Lock-dir records carry it instead of degrading to
/// `pid-{pid}` for native writes.
fn file_execution_id(ctx: &ToolContext) -> String {
    let token = match ctx.tool_call_id.as_deref() {
        Some(id) if !id.is_empty() && !id.chars().any(char::is_whitespace) => id.to_owned(),
        _ => uuid::Uuid::new_v4().simple().to_string(),
    };
    format!("{}:{}", std::process::id(), token)
}

/// JS `ctx.signal?.throwIfAborted()` as shaped by `toCodingError`.
fn throw_if_aborted(ctx: &ToolContext) -> Result<(), CodingError> {
    if ctx.cancellation.is_cancelled() {
        return Err(CodingError::new("aborted", "the call was aborted"));
    }
    Ok(())
}

struct FileRead {
    text: String,
    summary: String,
    truncated: bool,
    sha256: String,
    total_lines: usize,
    invalid_utf8: bool,
}

#[allow(clippy::too_many_arguments)]
async fn read_one(
    env: &PackEnv,
    root: &str,
    path: &str,
    offset: i64,
    limit: Option<i64>,
    max_bytes: usize,
    chat_id: Option<&str>,
) -> Result<FileRead, CodingError> {
    let target = confine(
        root,
        path,
        ConfineOptions {
            access: PathAccess::Read,
            allow_sensitive: env.allow_sensitive,
        },
    )
    .await?;
    let buffer = read_regular_file(&target.path, path, env.limits.read_max_file_bytes).await?;
    if looks_binary(&buffer) {
        return Err(CodingError::new(
            "binary_file",
            format!(
                "{path} looks binary ({}); not shown",
                format_size(buffer.len())
            ),
        ));
    }
    let digest = sha256(&buffer);
    let strict = decode_utf8(&buffer);
    let invalid_utf8 = strict.is_none();
    let content = strict.unwrap_or_else(|| String::from_utf8_lossy(&buffer).into_owned());
    let lines = split_lines(&content);
    if lines.is_empty() {
        ledger::record_read(chat_id, &target.path, &digest, true);
        return Ok(FileRead {
            text: "(empty file)".to_owned(),
            summary: format!("read {path} (empty)"),
            truncated: false,
            sha256: digest,
            total_lines: lines.len(),
            invalid_utf8,
        });
    }
    if offset > lines.len() as i64 {
        return Err(CodingError::new(
            "offset_out_of_range",
            format!(
                "offset {offset} is beyond the end of {path} ({} lines)",
                lines.len()
            ),
        ));
    }
    let wanted = std::cmp::min(
        limit.unwrap_or(env.limits.read_max_lines as i64),
        env.limits.read_max_lines as i64,
    ) as usize;
    let start = (offset - 1) as usize;
    let slice = &lines[start..std::cmp::min(start + wanted, lines.len())];
    let numbered = slice
        .iter()
        .enumerate()
        .map(|(i, line)| {
            format!(
                "{}\t{}",
                offset + i as i64,
                line.strip_suffix('\r').unwrap_or(line)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let cut = truncate_head(
        &numbered,
        TruncationLimits {
            max_lines: slice.len(),
            max_bytes,
        },
    );
    if cut.first_line_exceeds_limit {
        return Err(CodingError::new(
            "line_too_long",
            format!(
                "line {offset} of {path} exceeds {}; it cannot be shown",
                format_size(max_bytes)
            ),
        ));
    }
    let last = offset + cut.output_lines as i64 - 1;
    let more = last < lines.len() as i64;
    // Only a read that shows the whole file may later authorise an overwrite.
    ledger::record_read(
        chat_id,
        &target.path,
        &digest,
        offset == 1 && !more && !cut.truncated,
    );
    let notice = if more {
        format!(
            "\n[Showing lines {offset}-{last} of {}{}. Use offset={} to continue.]",
            lines.len(),
            if cut.truncated_by == Some(TruncatedBy::Bytes) {
                format!(" ({} limit)", format_size(max_bytes))
            } else {
                String::new()
            },
            last + 1
        )
    } else {
        String::new()
    };
    Ok(FileRead {
        text: format!("{}{}", cut.content, notice),
        summary: format!("read {path} lines {offset}-{last} of {}", lines.len()),
        truncated: cut.truncated || (limit.is_none() && more),
        sha256: digest,
        total_lines: lines.len(),
        invalid_utf8,
    })
}

fn read_paths(input: &Map<String, Value>) -> Result<Vec<String>, CodingError> {
    let single = input.get("path");
    let many = input.get("paths");
    if single.is_none() == many.is_none() {
        return Err(CodingError::new(
            "invalid_arguments",
            "pass exactly one of \"path\" or \"paths\"",
        ));
    }
    let Some(many) = many else {
        return Ok(vec![req_string(input, "path", 4096)?]);
    };
    let invalid = CodingError::new(
        "invalid_arguments",
        format!("\"paths\" must be 1..{MAX_READ_PATHS} non-empty strings"),
    );
    let Value::Array(items) = many else {
        return Err(invalid);
    };
    if items.is_empty() || items.len() > MAX_READ_PATHS {
        return Err(invalid);
    }
    let mut paths: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(value) = item else {
            return Err(invalid);
        };
        if value.is_empty() || value.encode_utf16().count() > 4096 {
            return Err(invalid);
        }
        paths.push(value.clone());
    }
    let mut unique = paths.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != paths.len() {
        return Err(CodingError::new(
            "invalid_arguments",
            "\"paths\" must be unique",
        ));
    }
    Ok(paths)
}

const INVALID_UTF8_WARNING: &str = "not valid UTF-8; shown with replacement characters";

pub fn read_file_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let description = format!(
        "Read text files in the workspace: one file with path, or up to {MAX_READ_PATHS} with \
         paths (each under a \"==> path <==\" header; the output cap is shared; a file that fails \
         is reported inline). Output lines are numbered \"<n>\t<line>\". Returns at most {} lines \
         per file and {} in total, and ends with a continuation notice (offset=N) when more \
         remains. offset is 1-based. The result data carries sha256 (of the whole file) and \
         totalLines: pass that sha256 as edit_file's expectedSha256. {NATIVE_HINT}",
        env.limits.read_max_lines,
        format_size(env.limits.read_max_bytes),
    );
    let env = Arc::clone(env);
    define_tool(
        ToolSpec {
            name: "read_file",
            effect: ToolEffect::Read,
            capability: "fs.read",
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the workspace root.",
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "maxItems": MAX_READ_PATHS,
                        "description": format!(
                            "Several files (1..{MAX_READ_PATHS}) instead of path."
                        ),
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based first line to read (applies to every file).",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of lines per file.",
                    },
                },
                "oneOf": [{ "required": ["path"] }, { "required": ["paths"] }],
                "additionalProperties": false,
            }),
            timeout_ms: env.limits.tool_timeout_ms,
        },
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            async move {
                let input = as_record(&raw)?;
                let paths = read_paths(input)?;
                let offset = opt_int(input, "offset", 1, 1_000_000_000)?.unwrap_or(1);
                let limit = opt_int(input, "limit", 1, 1_000_000_000)?;
                let root = env.root().await?;
                if input.get("path").is_some() {
                    throw_if_aborted(&ctx)?;
                    let one = read_one(
                        &env,
                        &root,
                        &paths[0],
                        offset,
                        limit,
                        env.limits.read_max_bytes,
                        ctx.chat_id.as_deref(),
                    )
                    .await?;
                    let mut details = json!({
                        "path": paths[0],
                        "sha256": one.sha256,
                        "totalLines": one.total_lines,
                    });
                    if one.invalid_utf8 {
                        details["encoding"] = json!("invalid-utf8");
                    }
                    return Ok(Outcome {
                        ok: true,
                        text: one.text,
                        summary: Some(one.summary),
                        truncated: one.truncated,
                        warnings: if one.invalid_utf8 {
                            vec![INVALID_UTF8_WARNING.to_owned()]
                        } else {
                            vec![]
                        },
                        details: Some(details),
                    });
                }
                let share = env.limits.read_max_bytes / paths.len();
                let mut blocks: Vec<String> = Vec::new();
                let mut files: Vec<Value> = Vec::new();
                let mut warnings: Vec<String> = Vec::new();
                let mut failed = 0usize;
                let mut truncated = false;
                for path in &paths {
                    throw_if_aborted(&ctx)?;
                    match read_one(
                        &env,
                        &root,
                        path,
                        offset,
                        limit,
                        share,
                        ctx.chat_id.as_deref(),
                    )
                    .await
                    {
                        Ok(one) => {
                            blocks.push(format!("==> {path} <==\n{}", one.text));
                            truncated = truncated || one.truncated;
                            if one.invalid_utf8 {
                                warnings.push(format!("{path}: {INVALID_UTF8_WARNING}"));
                            }
                            let mut file = json!({
                                "path": path,
                                "sha256": one.sha256,
                                "totalLines": one.total_lines,
                            });
                            if one.invalid_utf8 {
                                file["encoding"] = json!("invalid-utf8");
                            }
                            files.push(file);
                        }
                        Err(error) => {
                            failed += 1;
                            blocks
                                .push(format!("==> {path} <==\n{}: {}", error.code, error.message));
                            files.push(json!({ "path": path, "error": error.code }));
                        }
                    }
                }
                Ok(Outcome {
                    ok: failed < paths.len(),
                    text: blocks.join("\n\n"),
                    summary: Some(format!(
                        "read {} of {} files",
                        paths.len() - failed,
                        paths.len()
                    )),
                    truncated,
                    warnings,
                    details: Some(json!({ "files": files })),
                })
            }
        },
    )
}

pub fn list_dir_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let description = format!(
        "List a directory in the workspace (max {} entries; directories end with /, symlinks \
         with @; credential files and entries matched by .gitignore are hidden unless \
         includeIgnored is set). {NATIVE_HINT}",
        env.limits.list_max_entries,
    );
    let env = Arc::clone(env);
    define_tool(
        ToolSpec {
            name: "list_dir",
            effect: ToolEffect::Read,
            capability: "fs.read",
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory, relative to the workspace root. Default: the root.",
                    },
                    "includeIgnored": {
                        "type": "boolean",
                        "description": "Also list entries matched by .gitignore / .ignore / .git/info/exclude.",
                    },
                },
                "additionalProperties": false,
            }),
            timeout_ms: env.limits.tool_timeout_ms,
        },
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            async move {
                let input = as_record(&raw)?;
                let path = opt_string(input, "path", 4096)?.unwrap_or_else(|| ".".to_owned());
                let include_ignored = opt_bool(input, "includeIgnored")?.unwrap_or(false);
                throw_if_aborted(&ctx)?;
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
                let Some(info) = info else {
                    return Err(CodingError::new(
                        "not_found",
                        format!("{path} does not exist"),
                    ));
                };
                if !info.is_dir() {
                    return Err(CodingError::new(
                        "not_a_directory",
                        format!("{path} is not a directory"),
                    ));
                }
                let mut all: Vec<(String, bool, bool)> = Vec::new();
                let mut handle = tokio::fs::read_dir(&target.path)
                    .await
                    .map_err(|error| CodingError::new("io_error", error.to_string()))?;
                while let Some(entry) = handle
                    .next_entry()
                    .await
                    .map_err(|error| CodingError::new("io_error", error.to_string()))?
                {
                    let file_type = entry
                        .file_type()
                        .await
                        .map_err(|error| CodingError::new("io_error", error.to_string()))?;
                    all.push((
                        entry.file_name().to_string_lossy().into_owned(),
                        file_type.is_dir(),
                        file_type.is_symlink(),
                    ));
                }
                throw_if_aborted(&ctx)?;
                let matcher = if include_ignored {
                    None
                } else {
                    Some(load_ignore_matcher(&target.path, &root, &env.limits).await)
                };
                let permitted: Vec<&(String, bool, bool)> = all
                    .iter()
                    .filter(|(name, _, _)| env.allow_sensitive || !is_sensitive_segment(name))
                    .collect();
                let mut visible: Vec<&(String, bool, bool)> = permitted
                    .iter()
                    .copied()
                    .filter(|(name, is_dir, _)| {
                        !matcher
                            .as_ref()
                            .is_some_and(|m| m.is_ignored(name, *is_dir))
                    })
                    .collect();
                // JS `<` compares UTF-16 code units, not UTF-8 bytes; the
                // port mirrors UTF-16 semantics everywhere else, so the
                // listing (and the cap boundary) must order the same.
                visible.sort_by(|left, right| left.0.encode_utf16().cmp(right.0.encode_utf16()));
                let hidden = all.len() - permitted.len();
                let ignored = permitted.len() - visible.len();
                let shown = &visible[..std::cmp::min(visible.len(), env.limits.list_max_entries)];
                let mut lines: Vec<String> = shown
                    .iter()
                    .map(|(name, is_dir, is_link)| {
                        if *is_dir {
                            format!("{name}/")
                        } else if *is_link {
                            format!("{name}@")
                        } else {
                            name.clone()
                        }
                    })
                    .collect();
                if visible.len() > shown.len() {
                    lines.push(format!(
                        "[{} more entries not shown]",
                        visible.len() - shown.len()
                    ));
                }
                if hidden > 0 {
                    lines.push(format!("[{hidden} sensitive entries hidden]"));
                }
                if ignored > 0 {
                    lines.push(format!("[{ignored} ignored entries hidden]"));
                }
                let text = if lines.is_empty() {
                    "(empty directory)".to_owned()
                } else {
                    lines.join("\n")
                };
                Ok(Outcome {
                    ok: true,
                    text,
                    summary: Some(format!(
                        "listed {} ({} entries)",
                        if target.relative_path.is_empty() {
                            "."
                        } else {
                            &target.relative_path
                        },
                        shown.len()
                    )),
                    truncated: visible.len() > shown.len(),
                    warnings: vec![],
                    details: None,
                })
            }
        },
    )
}

fn parse_expected_sha(input: &Map<String, Value>) -> Result<Option<String>, CodingError> {
    let value = opt_string(input, "expectedSha256", 64)?;
    let Some(value) = value else {
        return Ok(None);
    };
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CodingError::new(
            "invalid_arguments",
            "\"expectedSha256\" must be 64 hex characters",
        ));
    }
    Ok(Some(value.to_lowercase()))
}

/// sha256 of the regular file at `path`, or None when it does not exist.
async fn read_existing(
    path: &str,
    label: &str,
    max_bytes: u64,
) -> Result<Option<String>, CodingError> {
    match read_regular_file(path, label, max_bytes).await {
        Ok(buffer) => Ok(Some(sha256(&buffer))),
        Err(error) if error.code == "not_found" => Ok(None),
        Err(error) => Err(error),
    }
}

/// Last check before a rename: the file must still be the one we validated.
async fn assert_unchanged(
    path: &str,
    label: &str,
    digest: &str,
    max_bytes: u64,
    relative_path: &str,
) -> Result<(), CodingError> {
    let now = match read_existing(path, label, max_bytes).await {
        Ok(now) => now,
        Err(error) if error.code == "file_changed" => {
            return Err(file_changed_error(relative_path));
        }
        Err(error) => return Err(error),
    };
    if now.as_deref() != Some(digest) {
        return Err(file_changed_error(relative_path));
    }
    Ok(())
}

pub fn write_file_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let description = format!(
        "Create or overwrite a text file inside the workspace (parent directories are created; \
         .git is refused; max {}). A new file is created only if it does not exist yet. \
         Overwriting an existing file requires that you read it first in this session (or pass \
         expectedSha256 from read_file); it is refused with file_changed if the file changed \
         since. For changing part of an existing file use edit_file instead.",
        format_size(env.limits.write_max_bytes),
    );
    let env = Arc::clone(env);
    define_tool(
        ToolSpec {
            name: "write_file",
            effect: ToolEffect::Write,
            capability: "fs.write",
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the workspace root.",
                    },
                    "content": { "type": "string", "description": "Full new file content." },
                    "expectedSha256": {
                        "type": "string",
                        "description": "sha256 (hex) the existing file must currently have. Default: the version this session last read.",
                    },
                },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
            timeout_ms: env.limits.tool_timeout_ms,
        },
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            async move {
                let input = as_record(&raw)?;
                let path = req_string(input, "path", 4096)?;
                let content = match input.get("content") {
                    Some(Value::String(content)) => content.clone(),
                    _ => {
                        return Err(CodingError::new(
                            "invalid_arguments",
                            "\"content\" must be a string",
                        ));
                    }
                };
                let explicit = parse_expected_sha(input)?;
                let data = content.into_bytes();
                if data.len() > env.limits.write_max_bytes {
                    return Err(CodingError::new(
                        "content_too_large",
                        format!(
                            "content is {} bytes; the limit is {}",
                            data.len(),
                            env.limits.write_max_bytes
                        ),
                    ));
                }
                let root = env.root().await?;
                let options = ConfineOptions {
                    access: PathAccess::Write,
                    allow_sensitive: env.allow_sensitive,
                };
                let first = confine(&root, &path, options).await?;
                let mut lease = env
                    .coordinator
                    .acquire_file(AcquireFileInput {
                        root: root.clone(),
                        path: first.path.clone(),
                        holder: holder_of(&ctx, "write_file", &first.relative_path),
                        cancellation: Some(ctx.cancellation.clone()),
                        max_wait_ms: Some(env.coordination_wait_ms),
                        execution_id: Some(file_execution_id(&ctx)),
                    })
                    .await?;
                let result =
                    write_file_body(&env, &ctx, &root, &path, &data, &explicit, &first, options)
                        .await;
                lease.release().await;
                result
            }
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn write_file_body(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    root: &str,
    path: &str,
    data: &[u8],
    explicit: &Option<String>,
    first: &ConfinedPath,
    options: ConfineOptions,
) -> Result<Outcome, CodingError> {
    throw_if_aborted(ctx)?;
    let parent = match first.path.rfind('/') {
        Some(at) => &first.path[..at],
        None => ".",
    };
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| CodingError::new("io_error", error.to_string()))?;
    // Re-resolve after mkdir: confirms no parent turned into an escape.
    let target = confine(root, path, options).await?;
    // One resolution per mutation: the lock, read, write and ledger all use
    // the locked path, so a path that now resolves elsewhere is refused.
    if target.path != first.path {
        return Err(file_changed_error(&first.relative_path));
    }
    throw_if_aborted(ctx)?;
    let read_limit = std::cmp::max(
        std::cmp::max(
            env.limits.edit_max_file_bytes,
            env.limits.read_max_file_bytes,
        ),
        env.limits.write_max_bytes as u64,
    );
    let current = read_existing(&target.path, path, read_limit).await?;
    let digest = sha256(data);
    let existed = current.is_some();
    if let Some(current) = current {
        let mut expected = explicit.clone();
        if expected.is_none() {
            let seen = ledger::expected(ctx.chat_id.as_deref(), &target.path);
            if seen.is_none() || seen.as_ref().is_some_and(|entry| !entry.full) {
                return Err(file_not_read_error(&target.relative_path, seen.is_some()));
            }
            expected = seen.map(|entry| entry.sha256);
        }
        if expected.as_deref() != Some(current.as_str()) {
            return Err(file_changed_error(&target.relative_path));
        }
        let verify = verify_callback(
            root,
            path,
            &target.path,
            &target.relative_path,
            &current,
            read_limit,
            options,
        );
        atomic_write(&target.path, data, path, Some(verify)).await?;
    } else {
        // An expected version means "overwrite that file"; it is gone.
        if explicit.is_some() {
            return Err(file_changed_error(&target.relative_path));
        }
        create_if_absent(&target.path, data, &target.relative_path).await?;
    }
    ledger::record_write(ctx.chat_id.as_deref(), &target.path, &digest);
    Ok(Outcome {
        ok: true,
        text: format!(
            "{} {} ({} bytes)\nsha256: {digest}",
            if existed { "Overwrote" } else { "Created" },
            target.relative_path,
            data.len()
        ),
        summary: Some(format!(
            "{} {}",
            if existed { "overwrote" } else { "created" },
            target.relative_path
        )),
        truncated: false,
        warnings: vec![],
        details: Some(json!({
            "path": target.relative_path,
            "sha256": digest,
        })),
    })
}

/// The `before_rename` guard shared by `write_file` and `edit_file`: the path
/// must still resolve to the locked target and the bytes must still match.
fn verify_callback(
    root: &str,
    path: &str,
    target_path: &str,
    relative_path: &str,
    digest: &str,
    max_bytes: u64,
    options: ConfineOptions,
) -> impl FnOnce() -> BoxFuture<'static, Result<(), CodingError>> {
    let root = root.to_owned();
    let path = path.to_owned();
    let target_path = target_path.to_owned();
    let relative_path = relative_path.to_owned();
    let digest = digest.to_owned();
    move || {
        let root = root.clone();
        let path = path.clone();
        let target_path = target_path.clone();
        let relative_path = relative_path.clone();
        let digest = digest.clone();
        Box::pin(async move {
            let again = confine(&root, &path, options).await?;
            if again.path != target_path {
                return Err(file_changed_error(&relative_path));
            }
            assert_unchanged(&target_path, &path, &digest, max_bytes, &relative_path).await
        })
    }
}

const MAX_EDITS: usize = 50;

fn parse_edits(value: Option<&Value>) -> Result<Vec<Edit>, CodingError> {
    let invalid = CodingError::new(
        "invalid_arguments",
        format!("\"edits\" must be an array of 1..{MAX_EDITS} {{oldText,newText}}"),
    );
    let Some(Value::Array(items)) = value else {
        return Err(invalid);
    };
    if items.is_empty() || items.len() > MAX_EDITS {
        return Err(invalid);
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let record = as_record(item)?;
            let old_text = match record.get("oldText") {
                Some(Value::String(value)) => value.clone(),
                _ => {
                    return Err(CodingError::new(
                        "invalid_arguments",
                        format!("edits[{i}] needs string oldText and newText"),
                    ));
                }
            };
            let new_text = match record.get("newText") {
                Some(Value::String(value)) => value.clone(),
                _ => {
                    return Err(CodingError::new(
                        "invalid_arguments",
                        format!("edits[{i}] needs string oldText and newText"),
                    ));
                }
            };
            Ok(Edit { old_text, new_text })
        })
        .collect()
}

pub fn edit_file_tool(env: &Arc<PackEnv>) -> Arc<dyn Tool> {
    let env = Arc::clone(env);
    define_tool(
        ToolSpec {
            name: "edit_file",
            effect: ToolEffect::Write,
            capability: "fs.write",
            description: "Edit an existing text file with exact text replacement. Pass edits[]; \
                          each oldText must match exactly once in the ORIGINAL file (whitespace, \
                          quotes and trailing spaces are matched leniently only when no exact \
                          match exists) and edits must not overlap. Keep oldText as small as \
                          possible while unique. Each line's original terminator (CRLF, LF or CR) \
                          and any BOM are preserved; new lines use the file's dominant ending. \
                          Files that are not valid UTF-8 are refused. The edit is refused with \
                          file_changed if the file changed since this session last read it (or \
                          since the optional expectedSha256). Read the file first. Returns a \
                          unified diff and the new sha256."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path, relative to the workspace root.",
                    },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_EDITS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {
                                    "type": "string",
                                    "description": "Exact text to replace; unique in the file.",
                                },
                                "newText": {
                                    "type": "string",
                                    "description": "Replacement text.",
                                },
                            },
                            "required": ["oldText", "newText"],
                            "additionalProperties": false,
                        },
                    },
                    "expectedSha256": {
                        "type": "string",
                        "description": "sha256 (hex) the file must currently have.",
                    },
                },
                "required": ["path", "edits"],
                "additionalProperties": false,
            }),
            timeout_ms: env.limits.tool_timeout_ms,
        },
        move |ctx: ToolContext, raw: Value| {
            let env = Arc::clone(&env);
            async move {
                let input = as_record(&raw)?;
                let path = req_string(input, "path", 4096)?;
                let edits = parse_edits(input.get("edits"))?;
                let explicit = parse_expected_sha(input)?;
                let root = env.root().await?;
                let options = ConfineOptions {
                    access: PathAccess::Write,
                    allow_sensitive: env.allow_sensitive,
                };
                let target = confine(&root, &path, options).await?;
                let mut lease = env
                    .coordinator
                    .acquire_file(AcquireFileInput {
                        root: root.clone(),
                        path: target.path.clone(),
                        holder: holder_of(&ctx, "edit_file", &target.relative_path),
                        cancellation: Some(ctx.cancellation.clone()),
                        max_wait_ms: Some(env.coordination_wait_ms),
                        execution_id: Some(file_execution_id(&ctx)),
                    })
                    .await?;
                let result = edit_file_body(
                    &env, &ctx, &root, &path, &edits, &explicit, &target, options,
                )
                .await;
                lease.release().await;
                result
            }
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn edit_file_body(
    env: &Arc<PackEnv>,
    ctx: &ToolContext,
    root: &str,
    path: &str,
    edits: &[Edit],
    explicit: &Option<String>,
    target: &ConfinedPath,
    options: ConfineOptions,
) -> Result<Outcome, CodingError> {
    throw_if_aborted(ctx)?;
    let buffer = read_regular_file(&target.path, path, env.limits.edit_max_file_bytes).await?;
    if looks_binary(&buffer) {
        return Err(CodingError::new(
            "binary_file",
            format!("{path} looks binary; not edited"),
        ));
    }
    let before = sha256(&buffer);
    // A partial read is enough here: edits are anchored on oldText.
    let expected = match explicit {
        Some(value) => Some(value.clone()),
        None => ledger::expected(ctx.chat_id.as_deref(), &target.path).map(|entry| entry.sha256),
    };
    if expected.is_some() && expected.as_deref() != Some(before.as_str()) {
        return Err(file_changed_error(&target.relative_path));
    }
    let decoded = decode_utf8(&buffer).ok_or_else(|| {
        CodingError::new(
            "encoding_unsupported",
            format!("{path} is not valid UTF-8; not edited (editing would corrupt it)"),
        )
    })?;
    let split = split_bom(&decoded);
    let applied = apply_edits(&split.text, edits, path)?;
    let out = format!("{}{}", split.bom, applied.output).into_bytes();
    throw_if_aborted(ctx)?;
    let verify = verify_callback(
        root,
        path,
        &target.path,
        &target.relative_path,
        &before,
        env.limits.edit_max_file_bytes,
        options,
    );
    atomic_write(&target.path, &out, path, Some(verify)).await?;
    let after = sha256(&out);
    ledger::record_write(ctx.chat_id.as_deref(), &target.path, &after);
    let patch = unified_diff(
        &target.relative_path,
        &applied.base_content,
        &applied.new_content,
    );
    let cut = truncate_head(
        &patch.diff,
        TruncationLimits {
            max_lines: usize::MAX,
            max_bytes: env.limits.edit_max_diff_bytes,
        },
    );
    let diff_text = if cut.truncated {
        format!(
            "{}\n[diff truncated at {}]",
            cut.content,
            format_size(env.limits.edit_max_diff_bytes)
        )
    } else {
        patch.diff
    };
    Ok(Outcome {
        ok: true,
        text: format!(
            "Edited {} ({} edit{}, +{} -{})\nsha256: {after}\n{diff_text}",
            target.relative_path,
            edits.len(),
            if edits.len() == 1 { "" } else { "s" },
            patch.added,
            patch.removed
        ),
        summary: Some(format!(
            "edited {} (+{} -{})",
            target.relative_path, patch.added, patch.removed
        )),
        truncated: cut.truncated,
        warnings: vec![],
        details: Some(json!({
            "path": target.relative_path,
            "sha256": after,
            "diff": diff_text,
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::CodingLimits;
    use crate::types::SandboxMode;
    use agentkit_core::{CancellationToken, Tool, ToolContext, ToolResult};
    use std::path::PathBuf;

    struct Harness {
        root: PathBuf,
        chat: String,
        read_file: Arc<dyn Tool>,
        list_dir: Arc<dyn Tool>,
        write_file: Arc<dyn Tool>,
        edit_file: Arc<dyn Tool>,
    }

    impl Harness {
        fn new(limits: CodingLimits) -> Self {
            let base = std::env::temp_dir().join(format!(
                "ak-fstools-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&base).unwrap();
            let root = std::fs::canonicalize(&base).unwrap();
            let env = Arc::new(PackEnv::new(crate::context::EnvInit {
                workspace_root: root.to_string_lossy().into_owned(),
                limits,
                allow_sensitive: false,
                clock: crate::types::system_clock(),
                sandbox: SandboxMode::Off,
                network: false,
                allow_git_writes: false,
                test_commands: vec![],
                platform: "linux".to_owned(),
                sandbox_exec_path: String::new(),
                coordinator: crate::coordination::create_in_process_workspace_coordinator(None),
                coordination_wait_ms: 5_000,
            }));
            Harness {
                chat: format!("chat-{}", uuid::Uuid::new_v4().simple()),
                read_file: read_file_tool(&env),
                list_dir: list_dir_tool(&env),
                write_file: write_file_tool(&env),
                edit_file: edit_file_tool(&env),
                root,
            }
        }

        fn put(&self, rel: &str, content: &[u8]) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }

        fn contents(&self, rel: &str) -> Vec<u8> {
            std::fs::read(self.root.join(rel)).unwrap()
        }

        fn remove(&self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }

        async fn run(&self, tool: &Arc<dyn Tool>, input: Value) -> ToolResult {
            self.run_with(
                tool,
                input,
                Some(self.chat.as_str()),
                CancellationToken::new(),
            )
            .await
        }

        async fn run_with(
            &self,
            tool: &Arc<dyn Tool>,
            input: Value,
            chat: Option<&str>,
            cancellation: CancellationToken,
        ) -> ToolResult {
            let context = ToolContext {
                suspension: None,
                run_id: "run-1".to_owned(),
                tool_call_id: None,
                attempt_id: None,
                chat_id: chat.map(str::to_owned),
                scope_id: None,
                user_id: None,
                bindings: vec![],
                limits: Default::default(),
                cancellation,
                dispatch_guard: None,
                metadata: Default::default(),
            };
            tool.execute(context, input).await.unwrap()
        }

        async fn read_file_with(&self, input: Value, chat: Option<&str>) -> ToolResult {
            self.run_with(&self.read_file, input, chat, CancellationToken::new())
                .await
        }

        fn text(result: &ToolResult) -> String {
            result
                .model_data
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        }

        fn summary(result: &ToolResult) -> String {
            result.summary.clone().unwrap_or_default()
        }
    }

    fn lines(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn read_numbers_lines_with_a_tab() {
        let h = Harness::new(CodingLimits::default());
        h.put("a.txt", b"alpha\nbeta\ngamma\n");
        let r = h.run(&h.read_file, json!({ "path": "a.txt" })).await;
        assert!(r.ok);
        assert_eq!(Harness::text(&r), "1\talpha\n2\tbeta\n3\tgamma");
        assert!(!r.truncated);
        let details = r.data.unwrap();
        assert_eq!(details["sha256"], json!(sha256(b"alpha\nbeta\ngamma\n")));
        assert_eq!(details["totalLines"], json!(3));
        assert_eq!(details["path"], json!("a.txt"));
        h.remove();
    }

    #[tokio::test]
    async fn read_offset_limit_and_continuation_notice() {
        let h = Harness::new(CodingLimits::default());
        h.put("a.txt", lines(10).as_bytes());
        let r = h
            .run(
                &h.read_file,
                json!({ "path": "a.txt", "offset": 3, "limit": 2 }),
            )
            .await;
        assert_eq!(
            Harness::text(&r),
            "3\tline 3\n4\tline 4\n[Showing lines 3-4 of 10. Use offset=5 to continue.]"
        );
        let tail = h
            .run(&h.read_file, json!({ "path": "a.txt", "offset": 9 }))
            .await;
        assert_eq!(Harness::text(&tail), "9\tline 9\n10\tline 10");
        h.remove();
    }

    #[tokio::test]
    async fn read_offset_past_end_fails() {
        let h = Harness::new(CodingLimits::default());
        h.put("a.txt", lines(3).as_bytes());
        let r = h
            .run(&h.read_file, json!({ "path": "a.txt", "offset": 4 }))
            .await;
        assert!(!r.ok);
        assert!(Harness::text(&r).contains("offset_out_of_range"));
        h.remove();
    }

    #[tokio::test]
    async fn read_stops_at_max_lines_with_notice() {
        let h = Harness::new(CodingLimits::default());
        h.put("big.txt", ["x"; 2500].join("\n").as_bytes());
        let r = h.run(&h.read_file, json!({ "path": "big.txt" })).await;
        assert!(r.ok);
        assert!(r.truncated);
        let text = Harness::text(&r);
        assert!(text.contains("2000\tx"));
        assert!(!text.contains("2001\tx"));
        assert!(text.ends_with("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"));
        let next = h
            .run(&h.read_file, json!({ "path": "big.txt", "offset": 2001 }))
            .await;
        assert!(Harness::text(&next).contains("2500\tx"));
        assert!(!next.truncated);
        h.remove();
    }

    #[tokio::test]
    async fn read_cuts_at_byte_limit_on_line_boundary() {
        let h = Harness::new(CodingLimits::default());
        h.put(
            "wide.txt",
            lines(100)
                .lines()
                .map(|_| "y".repeat(1000))
                .collect::<Vec<_>>()
                .join("\n")
                .as_bytes(),
        );
        let r = h.run(&h.read_file, json!({ "path": "wide.txt" })).await;
        let text = Harness::text(&r);
        assert!(r.truncated);
        assert!(text.len() < 51_200 + 200);
        assert!(text.contains("50.0KB limit"));
        assert!(text.ends_with("to continue.]"));
        h.remove();
    }

    #[tokio::test]
    async fn read_refuses_line_over_byte_limit() {
        let h = Harness::new(CodingLimits::default());
        h.put("one.txt", "z".repeat(60_000).as_bytes());
        let r = h.run(&h.read_file, json!({ "path": "one.txt" })).await;
        assert!(!r.ok);
        assert!(Harness::text(&r).contains("line_too_long"));
        h.remove();
    }

    #[tokio::test]
    async fn read_refuses_binary_missing_directory_and_oversized() {
        let h = Harness::new(CodingLimits {
            read_max_file_bytes: 100,
            ..CodingLimits::default()
        });
        h.put("b.bin", &[1, 2, 0, 3, 4]);
        let r = h.run(&h.read_file, json!({ "path": "b.bin" })).await;
        assert!(!r.ok);
        assert!(Harness::text(&r).contains("binary_file"));
        assert!(Harness::text(&r).contains("b.bin looks binary (5B); not shown"));
        let missing = h.run(&h.read_file, json!({ "path": "nope.txt" })).await;
        assert!(!missing.ok);
        assert_eq!(
            Harness::text(&missing),
            "not_found: nope.txt does not exist"
        );
        h.put("dir/x.txt", b"x");
        let dir = h.run(&h.read_file, json!({ "path": "dir" })).await;
        assert!(Harness::text(&dir).contains("not_a_file"));
        h.put("huge.txt", &[b'q'; 101]);
        let huge = h.run(&h.read_file, json!({ "path": "huge.txt" })).await;
        assert!(Harness::text(&huge).contains("file_too_large"));
        h.remove();
    }

    #[tokio::test]
    async fn read_empty_file_and_bad_arguments() {
        let h = Harness::new(CodingLimits::default());
        h.put("e.txt", b"");
        let empty = h.run(&h.read_file, json!({ "path": "e.txt" })).await;
        assert_eq!(Harness::text(&empty), "(empty file)");
        let bad = h
            .run(&h.read_file, json!({ "path": "e.txt", "offset": 0 }))
            .await;
        assert!(!bad.ok);
        assert!(Harness::text(&bad).contains("invalid_arguments"));
        let nul = h.run(&h.read_file, Value::Null).await;
        assert!(!nul.ok);
        let both = h
            .run(&h.read_file, json!({ "path": "e.txt", "paths": ["e.txt"] }))
            .await;
        assert!(!both.ok);
        assert!(Harness::text(&both).contains("pass exactly one of \"path\" or \"paths\""));
        let none = h.run(&h.read_file, json!({})).await;
        assert!(!none.ok);
        let dup = h
            .run(&h.read_file, json!({ "paths": ["e.txt", "e.txt"] }))
            .await;
        assert!(!dup.ok);
        assert!(Harness::text(&dup).contains("\"paths\" must be unique"));
        h.remove();
    }

    #[tokio::test]
    async fn read_paths_reports_missing_file_inline() {
        let h = Harness::new(CodingLimits::default());
        h.put("a.txt", b"alpha\n");
        h.put("b.txt", b"beta\n");
        let r = h
            .run(
                &h.read_file,
                json!({ "paths": ["a.txt", "nope.txt", "b.txt"] }),
            )
            .await;
        assert!(r.ok);
        let text = Harness::text(&r);
        assert!(text.contains("==> a.txt <==\n1\talpha"));
        assert!(text.contains("==> nope.txt <==\nnot_found"));
        assert!(text.contains("==> b.txt <==\n1\tbeta"));
        assert_eq!(Harness::summary(&r), "read 2 of 3 files");
        let all = h
            .run(&h.read_file, json!({ "paths": ["x.txt", "y.txt"] }))
            .await;
        assert!(!all.ok);
        h.remove();
    }

    #[tokio::test]
    async fn read_invalid_utf8_warns() {
        let h = Harness::new(CodingLimits::default());
        h.put("bad.txt", &[0x68, 0x69, 0xff, 0x0a]);
        let r = h.run(&h.read_file, json!({ "path": "bad.txt" })).await;
        assert!(r.ok);
        assert!(r.warnings.contains(&INVALID_UTF8_WARNING.to_owned()));
        assert_eq!(r.data.unwrap()["encoding"], json!("invalid-utf8"));
        h.remove();
    }

    #[tokio::test]
    async fn write_creates_parents_and_reports_sha256() {
        let h = Harness::new(CodingLimits::default());
        let r = h
            .run(
                &h.write_file,
                json!({ "path": "a/b/c.txt", "content": "hi\n" }),
            )
            .await;
        assert!(r.ok);
        assert_eq!(h.contents("a/b/c.txt"), b"hi\n");
        let text = Harness::text(&r);
        assert!(text.contains(&format!("sha256: {}", sha256(b"hi\n"))));
        assert!(text.starts_with("Created a/b/c.txt (3 bytes)"));
        assert_eq!(Harness::summary(&r), "created a/b/c.txt");
        let again = h
            .run(
                &h.write_file,
                json!({
                    "path": "a/b/c.txt",
                    "content": "yo\n",
                    "expectedSha256": sha256(b"hi\n"),
                }),
            )
            .await;
        assert!(again.ok);
        assert!(Harness::text(&again).starts_with("Overwrote a/b/c.txt"));
        assert_eq!(h.contents("a/b/c.txt"), b"yo\n");
        h.remove();
    }

    #[tokio::test]
    async fn write_overwrite_requires_prior_read() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"old\n");
        let r = h
            .run(
                &h.write_file,
                json!({ "path": "f.txt", "content": "new\n" }),
            )
            .await;
        assert!(!r.ok);
        assert_eq!(
            Harness::text(&r),
            "file_not_read: f.txt already exists. Read it before overwriting it."
        );
        // A full read in the same session authorises the overwrite.
        let read = h
            .read_file_with(json!({ "path": "f.txt" }), Some(&h.chat))
            .await;
        assert!(read.ok);
        let ok = h
            .run(
                &h.write_file,
                json!({ "path": "f.txt", "content": "new\n" }),
            )
            .await;
        assert!(ok.ok);
        assert_eq!(h.contents("f.txt"), b"new\n");
        h.remove();
    }

    #[tokio::test]
    async fn write_needs_a_full_read_not_a_partial_one() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"1\n2\n3\n");
        let partial = h
            .read_file_with(json!({ "path": "f.txt", "offset": 2 }), Some(&h.chat))
            .await;
        assert!(partial.ok);
        let r = h
            .run(
                &h.write_file,
                json!({ "path": "f.txt", "content": "new\n" }),
            )
            .await;
        assert!(!r.ok);
        assert_eq!(
            Harness::text(&r),
            "file_not_read: f.txt already exists and you only read part of it. \
             Read the whole file (no offset/limit, nothing truncated) or pass \
             expectedSha256 before overwriting it."
        );
        h.remove();
    }

    #[tokio::test]
    async fn write_other_chat_is_not_authorised() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"old\n");
        let read = h.read_file_with(json!({ "path": "f.txt" }), None).await;
        assert!(read.ok);
        let r = h
            .run(
                &h.write_file,
                json!({ "path": "f.txt", "content": "new\n" }),
            )
            .await;
        assert!(!r.ok);
        assert!(Harness::text(&r).starts_with("file_not_read"));
        h.remove();
    }

    #[tokio::test]
    async fn write_refuses_stale_hash_and_missing_expected() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"v1\n");
        let read = h
            .read_file_with(json!({ "path": "f.txt" }), Some(&h.chat))
            .await;
        assert!(read.ok);
        // External modification after the read: the ledger hash is stale.
        h.put("f.txt", b"tampered\n");
        let stale = h
            .run(
                &h.write_file,
                json!({ "path": "f.txt", "content": "new\n" }),
            )
            .await;
        assert!(!stale.ok);
        assert_eq!(
            Harness::text(&stale),
            "file_changed: f.txt changed since this session last read it \
             (another session or an external editor modified it). Read it \
             again and redo the edit."
        );
        // An explicit expected version for a file that is gone.
        std::fs::remove_file(h.root.join("g.txt")).ok();
        let gone = h
            .run(
                &h.write_file,
                json!({
                    "path": "g.txt",
                    "content": "x\n",
                    "expectedSha256": sha256(b"whatever\n"),
                }),
            )
            .await;
        assert!(!gone.ok);
        assert!(Harness::text(&gone).starts_with("file_changed"));
        h.remove();
    }

    #[tokio::test]
    async fn write_identical_content_succeeds_unchanged() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"same\n");
        let read = h
            .read_file_with(json!({ "path": "f.txt" }), Some(&h.chat))
            .await;
        assert!(read.ok);
        let r = h
            .run(
                &h.write_file,
                json!({ "path": "f.txt", "content": "same\n" }),
            )
            .await;
        assert!(r.ok);
        assert!(Harness::text(&r).starts_with("Overwrote f.txt (5 bytes)"));
        assert_eq!(h.contents("f.txt"), b"same\n");
        h.remove();
    }

    #[tokio::test]
    async fn write_refuses_bad_paths_and_content() {
        let h = Harness::new(CodingLimits {
            write_max_bytes: 10,
            ..CodingLimits::default()
        });
        let outside = h
            .run(
                &h.write_file,
                json!({ "path": "../esc.txt", "content": "x" }),
            )
            .await;
        assert!(Harness::text(&outside).contains("path_outside_root"));
        assert!(!h.root.join("..").join("esc.txt").exists());
        let git = h
            .run(
                &h.write_file,
                json!({ "path": ".git/HEAD", "content": "x" }),
            )
            .await;
        assert!(Harness::text(&git).contains("path_git_write"));
        let env_file = h
            .run(&h.write_file, json!({ "path": ".env", "content": "x" }))
            .await;
        assert!(Harness::text(&env_file).contains("path_sensitive"));
        let big = h
            .run(
                &h.write_file,
                json!({ "path": "big.txt", "content": "x".repeat(11) }),
            )
            .await;
        assert!(
            Harness::text(&big).contains("content_too_large: content is 11 bytes; the limit is 10")
        );
        let not_string = h
            .run(&h.write_file, json!({ "path": "n.txt", "content": 5 }))
            .await;
        assert_eq!(
            Harness::text(&not_string),
            "invalid_arguments: \"content\" must be a string"
        );
        let bad_sha = h
            .run(
                &h.write_file,
                json!({ "path": "n.txt", "content": "x", "expectedSha256": "zz" }),
            )
            .await;
        assert!(Harness::text(&bad_sha).contains("\"expectedSha256\" must be 64 hex characters"));
        h.remove();
    }

    #[tokio::test]
    async fn write_over_directory_fails_cleanly() {
        let h = Harness::new(CodingLimits::default());
        h.put("d/x.txt", b"x");
        let r = h
            .run(&h.write_file, json!({ "path": "d", "content": "x" }))
            .await;
        assert!(!r.ok);
        h.remove();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_create_race_reports_created_elsewhere() {
        // A raw writer slips in between `read_existing` seeing no file and
        // `create_if_absent` linking the temp file: the link fails EEXIST.
        let h = Harness::new(CodingLimits::default());
        for attempt in 0..8u32 {
            let name = format!("race{attempt}.txt");
            let target = h.root.join(&name);
            let seed_target = target.clone();
            let temp_parent = h.root.clone();
            let temp_prefix = format!(".{name}.");
            let tool = Arc::clone(&h.write_file);
            let chat = h.chat.clone();
            let content = json!({ "path": &name, "content": "A\n" });
            let seeder = tokio::spawn(async move {
                for _ in 0..4000 {
                    let Ok(mut dir) = tokio::fs::read_dir(&temp_parent).await else {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        continue;
                    };
                    let mut saw_temp = false;
                    while let Ok(Some(entry)) = dir.next_entry().await {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.starts_with(&temp_prefix) && name.ends_with(".tmp") {
                            saw_temp = true;
                        }
                    }
                    if saw_temp {
                        // The temp is on disk; race the link with a plain create.
                        let _ = tokio::fs::write(&seed_target, b"B\n").await;
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                false
            });
            let context = ToolContext {
                suspension: None,
                run_id: "run-race".to_owned(),
                tool_call_id: None,
                attempt_id: None,
                chat_id: Some(chat),
                scope_id: None,
                user_id: None,
                bindings: vec![],
                limits: Default::default(),
                cancellation: CancellationToken::new(),
                dispatch_guard: None,
                metadata: Default::default(),
            };
            let result = tool.execute(context, content).await.unwrap();
            let saw_temp = seeder.await.unwrap();
            if saw_temp && !result.ok {
                let text = Harness::text(&result);
                // `failure()` renders exactly `code: message`.
                let expected = format!(
                    "file_changed: {name} was created by someone else while writing it. \
                     Read it, then edit or overwrite it."
                );
                if text == expected {
                    assert_eq!(h.contents(&name), b"B\n");
                    h.remove();
                    return;
                }
                // The seed won the earlier window (file_not_read); retry.
            }
            // Lost the race window; retry with a fresh file name.
            let _ = std::fs::remove_file(&target);
        }
        h.remove();
        panic!("could not force the create_if_absent race");
    }

    #[tokio::test]
    async fn write_aborted_signal_leaves_file_untouched() {
        let h = Harness::new(CodingLimits::default());
        h.put("ab.txt", b"keep\n");
        let token = CancellationToken::new();
        token.cancel();
        let r = h
            .run_with(
                &h.write_file,
                json!({ "path": "ab.txt", "content": "lost" }),
                Some(&h.chat),
                token,
            )
            .await;
        assert!(!r.ok);
        assert!(Harness::text(&r).contains("aborted"));
        assert_eq!(h.contents("ab.txt"), b"keep\n");
        h.remove();
    }

    #[tokio::test]
    async fn edit_single_returns_diff_and_new_sha256() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"one\ntwo\nthree\n");
        let r = h
            .run(
                &h.edit_file,
                json!({
                    "path": "f.txt",
                    "edits": [{ "oldText": "two", "newText": "TWO" }],
                }),
            )
            .await;
        assert!(r.ok);
        assert_eq!(h.contents("f.txt"), b"one\nTWO\nthree\n");
        let text = Harness::text(&r);
        assert!(text.starts_with("Edited f.txt (1 edit, +1 -1)"));
        assert!(text.contains(&format!("sha256: {}", sha256(b"one\nTWO\nthree\n"))));
        assert!(text.contains("--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@"));
        assert!(text.contains("\n one\n-two\n+TWO\n three"));
        assert_eq!(Harness::summary(&r), "edited f.txt (+1 -1)");
        h.remove();
    }

    #[tokio::test]
    async fn edit_multiple_disjoint_match_the_original() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"a\nb\nc\nd\n");
        let r = h
            .run(
                &h.edit_file,
                json!({
                    "path": "f.txt",
                    "edits": [
                        { "oldText": "a", "newText": "b-new" },
                        { "oldText": "d", "newText": "a" },
                    ],
                }),
            )
            .await;
        assert!(r.ok);
        assert_eq!(h.contents("f.txt"), b"b-new\nb\nc\na\n");
        assert!(Harness::text(&r).starts_with("Edited f.txt (2 edits, "));
        h.remove();
    }

    #[tokio::test]
    async fn edit_no_match_and_ambiguous_are_refused() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"hello\n");
        let miss = h
            .run(
                &h.edit_file,
                json!({
                    "path": "f.txt",
                    "edits": [{ "oldText": "absent", "newText": "x" }],
                }),
            )
            .await;
        assert!(!miss.ok);
        assert!(Harness::text(&miss).contains("edit_no_match"));
        assert_eq!(h.contents("f.txt"), b"hello\n");

        h.put("x.txt", b"x = 1\nx = 1\n");
        let ambiguous = h
            .run(
                &h.edit_file,
                json!({
                    "path": "x.txt",
                    "edits": [{ "oldText": "x = 1", "newText": "x = 2" }],
                }),
            )
            .await;
        assert!(!ambiguous.ok);
        assert!(Harness::text(&ambiguous).contains("edit_ambiguous"));
        assert_eq!(h.contents("x.txt"), b"x = 1\nx = 1\n");
        h.remove();
    }

    #[tokio::test]
    async fn edit_refuses_stale_base() {
        let h = Harness::new(CodingLimits::default());
        h.put("f.txt", b"v1\n");
        let bad = h
            .run(
                &h.edit_file,
                json!({
                    "path": "f.txt",
                    "expectedSha256": sha256(b"something else"),
                    "edits": [{ "oldText": "v1", "newText": "v2" }],
                }),
            )
            .await;
        assert!(!bad.ok);
        assert!(Harness::text(&bad).starts_with("file_changed"));
        assert_eq!(h.contents("f.txt"), b"v1\n");
        let good = h
            .run(
                &h.edit_file,
                json!({
                    "path": "f.txt",
                    "expectedSha256": sha256(b"v1\n"),
                    "edits": [{ "oldText": "v1", "newText": "v2" }],
                }),
            )
            .await;
        assert!(good.ok);
        assert_eq!(h.contents("f.txt"), b"v2\n");

        // Ledger-backed staleness: read, external edit, then the edit fails.
        h.put("g.txt", b"one\n");
        let read = h
            .read_file_with(json!({ "path": "g.txt" }), Some(&h.chat))
            .await;
        assert!(read.ok);
        let read_sha = read.data.as_ref().unwrap()["sha256"]
            .as_str()
            .unwrap()
            .to_owned();
        let ok = h
            .run(
                &h.edit_file,
                json!({
                    "path": "g.txt",
                    "edits": [{ "oldText": "one", "newText": "ONE" }],
                    "expectedSha256": read_sha,
                }),
            )
            .await;
        assert!(ok.ok);
        let stale = h
            .run(
                &h.edit_file,
                json!({
                    "path": "g.txt",
                    "edits": [{ "oldText": "ONE", "newText": "uno" }],
                    "expectedSha256": read_sha,
                }),
            )
            .await;
        assert!(!stale.ok);
        assert!(Harness::text(&stale).contains("file_changed"));
        h.remove();
    }

    #[tokio::test]
    async fn edit_refuses_missing_binary_sensitive_and_malformed() {
        let h = Harness::new(CodingLimits::default());
        let edits = json!([{ "oldText": "a", "newText": "b" }]);
        let missing = h
            .run(&h.edit_file, json!({ "path": "nope.txt", "edits": edits }))
            .await;
        assert!(Harness::text(&missing).contains("not_found"));
        h.put("x.bin", b"a\0b");
        let binary = h
            .run(&h.edit_file, json!({ "path": "x.bin", "edits": edits }))
            .await;
        assert_eq!(
            Harness::text(&binary),
            "binary_file: x.bin looks binary; not edited"
        );
        h.put(".env", b"a");
        let sensitive = h
            .run(&h.edit_file, json!({ "path": ".env", "edits": edits }))
            .await;
        assert!(Harness::text(&sensitive).contains("path_sensitive"));
        assert_eq!(h.contents(".env"), b"a");
        h.put("f.txt", b"a");
        let empty_edits = h
            .run(&h.edit_file, json!({ "path": "f.txt", "edits": [] }))
            .await;
        assert_eq!(
            Harness::text(&empty_edits),
            "invalid_arguments: \"edits\" must be an array of 1..50 {oldText,newText}"
        );
        let malformed = h
            .run(
                &h.edit_file,
                json!({ "path": "f.txt", "edits": [{ "oldText": 1 }] }),
            )
            .await;
        assert!(Harness::text(&malformed).contains("edits[0] needs string oldText and newText"));
        let outside = h
            .run(
                &h.edit_file,
                json!({ "path": "../outside.txt", "edits": edits }),
            )
            .await;
        assert!(Harness::text(&outside).contains("path_outside_root"));
        h.remove();
    }

    #[tokio::test]
    async fn edit_aborted_signal_leaves_file_untouched() {
        let h = Harness::new(CodingLimits::default());
        h.put("ab.txt", b"keep\n");
        let token = CancellationToken::new();
        token.cancel();
        let r = h
            .run_with(
                &h.edit_file,
                json!({
                    "path": "ab.txt",
                    "edits": [{ "oldText": "keep", "newText": "lost" }],
                }),
                Some(&h.chat),
                token,
            )
            .await;
        assert!(!r.ok);
        assert!(Harness::text(&r).contains("aborted"));
        assert_eq!(h.contents("ab.txt"), b"keep\n");
        h.remove();
    }

    #[tokio::test]
    async fn list_dir_marks_hides_and_caps() {
        let h = Harness::new(CodingLimits {
            list_max_entries: 3,
            ..CodingLimits::default()
        });
        h.put("a/x.txt", b"x");
        h.put("b.txt", b"b");
        h.put("c.txt", b"c");
        h.put("d.txt", b"d");
        h.put(".env", b"A=1");
        let r = h.run(&h.list_dir, json!({})).await;
        assert!(r.ok);
        assert_eq!(
            Harness::text(&r),
            "a/\nb.txt\nc.txt\n[1 more entries not shown]\n\
             [1 sensitive entries hidden]"
        );
        assert!(r.truncated);
        assert_eq!(Harness::summary(&r), "listed . (3 entries)");
        let sub = h.run(&h.list_dir, json!({ "path": "a" })).await;
        assert_eq!(Harness::text(&sub), "x.txt");
        std::fs::create_dir(h.root.join("empty")).unwrap();
        let empty = h.run(&h.list_dir, json!({ "path": "empty" })).await;
        assert_eq!(Harness::text(&empty), "(empty directory)");
        let missing = h.run(&h.list_dir, json!({ "path": "nope" })).await;
        assert_eq!(Harness::text(&missing), "not_found: nope does not exist");
        let not_dir = h.run(&h.list_dir, json!({ "path": "b.txt" })).await;
        assert_eq!(
            Harness::text(&not_dir),
            "not_a_directory: b.txt is not a directory"
        );
        h.remove();
    }

    #[tokio::test]
    async fn list_dir_orders_by_utf16_code_units_like_js() {
        // JS `<` orders by UTF-16 code units: z(0x007A) < 😀(0xD83D..) <
        // \u{e000}(0xE000). UTF-8 byte order would flip the last two and
        // change which entries survive the list_max_entries cap.
        let h = Harness::new(CodingLimits {
            list_max_entries: 2,
            ..CodingLimits::default()
        });
        h.put("\u{e000}.txt", b"pua");
        h.put("\u{1f600}.txt", b"emoji");
        h.put("z.txt", b"z");
        let r = h.run(&h.list_dir, json!({})).await;
        assert!(r.ok);
        assert_eq!(
            Harness::text(&r),
            "z.txt\n\u{1f600}.txt\n[1 more entries not shown]"
        );
        h.remove();
    }
}
