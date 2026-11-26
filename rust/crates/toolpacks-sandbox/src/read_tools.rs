//! The read tools: `sandbox_read_file`, `sandbox_list_files`,
//! `sandbox_get_status`. They pass no ledger and take no lease (spec
//! "Reads"); they may run beside a command and see a half-written file. A
//! never-used attachment creates nothing (F-16).

use agentkit_core::{ToolContext, ToolLimits, ToolResult};
use agentkit_workspaces::{
    EntryKind, FileChunk, FileRef, LIST_DEPTH_MAX, LIST_DEPTH_MIN, Listing, RelPath,
};
use serde_json::{Map, Value, json};

use crate::defs::{MODEL_MAX_LINES, READ_MAX_BYTES};
use crate::envelope::{Refusal, failed, ok};
use crate::paths::{Roots, file_ref, guest_path, root_path};
use crate::ports::SandboxBinding;
use crate::tool::Staged;

mod status;

pub(crate) use status::status;

fn refused(refusal: &Refusal, limits: &ToolLimits) -> ToolResult {
    failed(&refusal.code, &refusal.message, json!({}), limits)
}

/// The attachment, and the model's path as a file below it.
async fn target(staged: &Staged, text: Option<&str>) -> Result<(SandboxBinding, FileRef), Refusal> {
    let binding = staged.attached().await?;
    let folder = binding.binding.folder.as_ref();
    let file = match text {
        Some(text) => file_ref(text, folder, Roots::Both)?,
        None => FileRef::workspace(RelPath::root()),
    };
    Ok((binding, file))
}

/// `sandbox_read_file`.
pub(crate) async fn read_file(staged: &Staged, context: &ToolContext, input: &Value) -> ToolResult {
    let limits = &context.limits;
    let Some(text) = input["file_path"].as_str() else {
        return refused(
            &Refusal::new("invalid_arguments", "file_path must be a string"),
            limits,
        );
    };
    let (binding, file) = match target(staged, Some(text)).await {
        Ok(found) => found,
        Err(refusal) => return refused(&refusal, limits),
    };
    let path = guest_path(binding.binding.folder.as_ref(), &file);
    let offset = input["offset"].as_u64().unwrap_or(0);
    let limit = input["limit"]
        .as_u64()
        .unwrap_or(READ_MAX_BYTES)
        .min(READ_MAX_BYTES);
    let service = &staged.shared.service;
    match service
        .read_file(&binding.binding, &binding.agent_id, &file, offset, limit)
        .await
    {
        Ok(chunk) => chunk_result(&path, offset, chunk, limits),
        Err(error) => failed(
            error.code(),
            &error.to_string(),
            json!({"path": path}),
            limits,
        ),
    }
}

/// The bytes a reply keeps: at most `MODEL_MAX_LINES` lines, and never a
/// UTF-8 sequence cut at the end (the next read starts where this one
/// stopped).
fn kept(bytes: &[u8], whole: bool) -> &[u8] {
    let mut end = bytes.len();
    if let Some((index, _)) = bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(MODEL_MAX_LINES - 1)
    {
        end = index + 1;
    }
    if end == bytes.len() && whole {
        return bytes;
    }
    match std::str::from_utf8(&bytes[..end]) {
        Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => {
            &bytes[..error.valid_up_to()]
        }
        _ => &bytes[..end],
    }
}

fn chunk_result(path: &str, offset: u64, chunk: FileChunk, limits: &ToolLimits) -> ToolResult {
    let mut data = Map::new();
    data.insert("path".into(), json!(path));
    data.insert("size".into(), json!(chunk.size));
    data.insert("mtimeMs".into(), json!(chunk.mtime_ms));
    data.insert("binary".into(), json!(chunk.binary));
    if chunk.binary {
        let text = format!(
            "{path} is a binary file of {} bytes; only its type and size are shown. Export it with sandbox_export_file or inspect it with a command.",
            chunk.size
        );
        return ok(text, data, limits);
    }
    let shown = kept(&chunk.bytes, chunk.eof);
    let next = offset + shown.len() as u64;
    let mut text = String::from_utf8_lossy(shown).into_owned();
    data.insert("offset".into(), json!(offset));
    data.insert("bytes".into(), json!(shown.len()));
    if next < chunk.size {
        data.insert("nextOffset".into(), json!(next));
        text.push_str(&format!(
            "\n[bytes {offset} to {next} of {}; continue with offset {next}]",
            chunk.size
        ));
    } else if shown.is_empty() {
        text = format!("[end of file: {path} has {} bytes]", chunk.size);
    }
    let mut done = ok(text, data, limits);
    done.truncated = next < chunk.size;
    done
}

/// `sandbox_list_files`.
pub(crate) async fn list_files(
    staged: &Staged,
    context: &ToolContext,
    input: &Value,
) -> ToolResult {
    let limits = &context.limits;
    let (binding, dir) = match target(staged, input["path"].as_str()).await {
        Ok(found) => found,
        Err(refusal) => return refused(&refusal, limits),
    };
    let depth = input["depth"]
        .as_u64()
        .and_then(|depth| u8::try_from(depth).ok())
        .unwrap_or(LIST_DEPTH_MIN)
        .clamp(LIST_DEPTH_MIN, LIST_DEPTH_MAX);
    let service = &staged.shared.service;
    match service
        .list(&binding.binding, &binding.agent_id, &dir, depth)
        .await
    {
        Ok(listing) => listing_result(
            binding.binding.folder.as_ref(),
            &dir,
            depth,
            listing,
            limits,
        ),
        Err(error) => failed(error.code(), &error.to_string(), json!({}), limits),
    }
}

fn entry_line(shown: &str, kind: EntryKind, size: Option<u64>) -> String {
    match kind {
        EntryKind::Directory => format!("{shown}/"),
        EntryKind::File => format!("{shown}  {} B", size.unwrap_or(0)),
        EntryKind::Symlink => format!("{shown}  (link, not followed)"),
        EntryKind::Other => format!("{shown}  (special file)"),
    }
}

fn listing_result(
    folder: Option<&RelPath>,
    dir: &FileRef,
    depth: u8,
    listing: Listing,
    limits: &ToolLimits,
) -> ToolResult {
    let base = guest_path(folder, dir);
    let mut lines = vec![format!("{base} (depth {depth}):")];
    let mut entries = vec![];
    for entry in &listing.entries {
        let absolute = root_path(dir.root, &entry.path);
        let shown = absolute
            .strip_prefix(&base)
            .map(|rest| rest.trim_start_matches('/'))
            .filter(|rest| !rest.is_empty())
            .unwrap_or(&absolute);
        lines.push(entry_line(shown, entry.kind, entry.size));
        entries.push(json!({"path": absolute, "kind": entry.kind, "size": entry.size}));
    }
    if listing.entries.is_empty() {
        lines.push("(empty)".to_owned());
    }
    if listing.truncated {
        lines.push("[more entries were left out; list a subfolder or a smaller depth]".to_owned());
    }
    let mut data = Map::new();
    data.insert("path".into(), json!(base));
    data.insert("depth".into(), json!(depth));
    data.insert("entries".into(), Value::Array(entries));
    data.insert("truncated".into(), json!(listing.truncated));
    let mut done = ok(lines.join("\n"), data, limits);
    done.truncated = listing.truncated;
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_keeps_at_most_the_line_cap_and_whole_utf8() {
        let lines = "x\n".repeat(MODEL_MAX_LINES + 5);
        assert_eq!(kept(lines.as_bytes(), true).len(), 2 * MODEL_MAX_LINES);
        let cut = "aé".as_bytes();
        assert_eq!(
            kept(&cut[..2], false),
            b"a",
            "a split character waits for the next read"
        );
        assert_eq!(kept(b"abc", true), b"abc");
    }
}
