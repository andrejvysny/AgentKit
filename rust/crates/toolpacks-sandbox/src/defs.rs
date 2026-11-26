//! The six tool definitions: names, effects, strict schemas (closed, with
//! bounds; they pass the runtime schema lint), descriptions and examples.
//! No schema has a workspace argument: the workspace is the host's pinned
//! attachment (SBX-11).

use agentkit_core::ToolDefinition;
use agentkit_workspaces::{
    EXEC_TIMEOUT_DEFAULT_MS, EXEC_TIMEOUT_MAX_MS, LEASE_WAIT_MS, LIST_DEPTH_MAX, LIST_DEPTH_MIN,
    READ_FILE_MAX_BYTES, WRITE_FILE_MAX_BYTES,
};
use serde_json::{Value, json};

pub(crate) const RUN: &str = "sandbox_run";
pub(crate) const READ_FILE: &str = "sandbox_read_file";
pub(crate) const WRITE_FILE: &str = "sandbox_write_file";
pub(crate) const LIST_FILES: &str = "sandbox_list_files";
pub(crate) const EXPORT_FILE: &str = "sandbox_export_file";
pub(crate) const GET_STATUS: &str = "sandbox_get_status";

/// Bytes one read returns at most (the model output cap).
pub(crate) const READ_MAX_BYTES: u64 = 50 * 1024;
/// Lines one read or one command tail shows at most.
pub(crate) const MODEL_MAX_LINES: usize = 2_000;
const PATH_MAX_CHARS: u64 = 4_096;
const COMMAND_MAX_CHARS: u64 = 65_536;
/// Longest export title: it is shown on the approval card.
pub(crate) const TITLE_MAX_CHARS: u64 = 200;

/// Which of the six tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ToolKind {
    Run,
    ReadFile,
    WriteFile,
    ListFiles,
    ExportFile,
    GetStatus,
}

impl ToolKind {
    pub(crate) const ALL: [ToolKind; 6] = [
        ToolKind::Run,
        ToolKind::ReadFile,
        ToolKind::WriteFile,
        ToolKind::ListFiles,
        ToolKind::ExportFile,
        ToolKind::GetStatus,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            ToolKind::Run => RUN,
            ToolKind::ReadFile => READ_FILE,
            ToolKind::WriteFile => WRITE_FILE,
            ToolKind::ListFiles => LIST_FILES,
            ToolKind::ExportFile => EXPORT_FILE,
            ToolKind::GetStatus => GET_STATUS,
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<ToolKind> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Write-effect tools: they pass the durable ledger and the mode table.
    pub(crate) fn changes(self) -> bool {
        matches!(
            self,
            ToolKind::Run | ToolKind::WriteFile | ToolKind::ExportFile
        )
    }

    /// Tools whose effect is a sandbox operation: a refusal before its row
    /// is reported as not started.
    pub(crate) fn operates(self) -> bool {
        matches!(self, ToolKind::Run | ToolKind::WriteFile)
    }

    /// Own capability strings, not `process.exec`/`fs.*`: the sandbox mode
    /// table (D-13) is not the coding matrix, and a capability-keyed host
    /// guard must not judge a sandbox command as a host command.
    fn capability(self) -> &'static str {
        match self {
            ToolKind::Run => "sandbox.exec",
            ToolKind::WriteFile => "sandbox.write",
            ToolKind::ExportFile => "sandbox.export",
            ToolKind::ReadFile | ToolKind::ListFiles | ToolKind::GetStatus => "sandbox.read",
        }
    }

    /// Longest a call may take: a run's longest deadline plus the lease
    /// wait and machine start; the rest the lease wait plus backend calls.
    fn timeout_ms(self) -> u64 {
        match self {
            ToolKind::Run => EXEC_TIMEOUT_MAX_MS + LEASE_WAIT_MS + 60_000,
            ToolKind::WriteFile | ToolKind::ExportFile => LEASE_WAIT_MS + 100_000,
            ToolKind::ReadFile | ToolKind::ListFiles | ToolKind::GetStatus => 60_000,
        }
    }

    pub(crate) fn aliases(self) -> &'static [(&'static str, &'static str)] {
        match self {
            ToolKind::Run => &[("timeoutMs", "timeout")],
            _ => &[],
        }
    }

    pub(crate) fn definition(self) -> ToolDefinition {
        let (description, input_schema, examples) = match self {
            ToolKind::Run => run(),
            ToolKind::ReadFile => read_file(),
            ToolKind::WriteFile => write_file(),
            ToolKind::ListFiles => list_files(),
            ToolKind::ExportFile => export_file(),
            ToolKind::GetStatus => get_status(),
        };
        ToolDefinition {
            name: self.name().to_owned(),
            version: "1".to_owned(),
            effect: if self.changes() { "write" } else { "read" }.to_owned(),
            capability: self.capability().to_owned(),
            description: description.to_owned(),
            input_schema,
            output_schema: None,
            timeout_ms: Some(self.timeout_ms()),
            examples,
        }
    }
}

type Parts = (&'static str, Value, Vec<Value>);

fn closed(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn file_path(verb: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": PATH_MAX_CHARS,
        "description": format!("File to {verb}: relative to your working folder, or absolute under /workspace or /home/agent (~/ also works).")
    })
}

fn run() -> Parts {
    let schema = closed(
        json!({
            "command": {"type": "string", "minLength": 1, "maxLength": COMMAND_MAX_CHARS,
                "description": "Shell command line, run with bash in a non-login shell."},
            "directory": {"type": "string", "minLength": 1, "maxLength": PATH_MAX_CHARS,
                "description": "Working directory: relative to your working folder, or absolute under /workspace. Default: the working folder."},
            "timeout": {"type": "integer", "minimum": 1000, "maximum": EXEC_TIMEOUT_MAX_MS,
                "description": format!("Deadline in milliseconds. Default {EXEC_TIMEOUT_DEFAULT_MS}.")}
        }),
        &["command"],
    );
    let examples = vec![
        json!({"command": "python3 analyze.py input/data.csv"}),
        json!({"command": "pip install --user pandas", "timeout": 600000}),
        json!({"command": "npm test", "directory": "app"}),
    ];
    (
        "Run a shell command (bash) in your own Linux sandbox, in your working folder under /workspace. Use it to run Python or Node scripts, install packages with pip install --user or npm install, and process data. Do not use it to read, list or write single files: sandbox_read_file, sandbox_list_files and sandbox_write_file are cheaper. The network is closed except for approved hosts. Returns the exit code and the last 2,000 lines or 50 KB of output; the full log stays in the sandbox. Default timeout 120 s, at most 3,600 s; timeout is in milliseconds.",
        schema,
        examples,
    )
}

fn read_file() -> Parts {
    let schema = closed(
        json!({
            "file_path": file_path("read"),
            "offset": {"type": "integer", "minimum": 0, "maximum": READ_FILE_MAX_BYTES,
                "description": "Byte position to start at. Default 0."},
            "limit": {"type": "integer", "minimum": 1, "maximum": READ_MAX_BYTES,
                "description": format!("Most bytes to return. Default {READ_MAX_BYTES}.")}
        }),
        &["file_path"],
    );
    let examples = vec![
        json!({"file_path": "report.md"}),
        json!({"file_path": "input/data.csv", "offset": 51200, "limit": 51200}),
    ];
    (
        "Read a text file in your sandbox. Use it to inspect files you or a command created, and files the user gave you under input/. Do not use it for files on the user's computer: only the sandbox is visible. Paths are relative to your working folder, or absolute under /workspace or /home/agent. Returns up to 2,000 lines or 50 KB from a byte offset, with the file size and the offset to continue from when more remains; a binary file reports its type and size only.",
        schema,
        examples,
    )
}

fn write_file() -> Parts {
    let schema = closed(
        json!({
            "file_path": file_path("write"),
            "content": {"type": "string", "maxLength": WRITE_FILE_MAX_BYTES,
                "description": "The complete new text of the file, at most 5 MiB."}
        }),
        &["file_path", "content"],
    );
    let examples = vec![json!({"file_path": "analyze.py", "content": "print('hello')\n"})];
    (
        "Write a whole text file in your sandbox, replacing it atomically and creating missing folders. Use it to create scripts, data and configuration under your working folder or /home/agent. Do not use it to change a large file in place; run a command for that instead. At most 5 MiB. Returns the path written, its size in bytes and its SHA-256.",
        schema,
        examples,
    )
}

fn list_files() -> Parts {
    let schema = closed(
        json!({
            "path": {"type": "string", "minLength": 1, "maxLength": PATH_MAX_CHARS,
                "description": "Folder to list: relative to your working folder, or absolute under /workspace or /home/agent. Default: the working folder."},
            "depth": {"type": "integer", "minimum": LIST_DEPTH_MIN, "maximum": LIST_DEPTH_MAX,
                "description": "Levels to descend. Default 1."}
        }),
        &[],
    );
    let examples = vec![json!({}), json!({"path": "input", "depth": 2})];
    (
        "List files and folders in your sandbox, up to 4 levels deep. Use it to see what exists before reading a file or running a command. Do not use it for the user's computer: only the sandbox is visible. Returns at most 500 entries, folders marked with a trailing slash and files with their size in bytes.",
        schema,
        examples,
    )
}

fn export_file() -> Parts {
    let schema = closed(
        json!({
            "file_path": file_path("export"),
            "title": {"type": "string", "minLength": 1, "maxLength": TITLE_MAX_CHARS,
                "description": "Name the user sees. Default: the file name."}
        }),
        &["file_path"],
    );
    let examples = vec![json!({"file_path": "out/report.pdf", "title": "Quarterly report"})];
    (
        "Give a file from your sandbox to the user as a file or artifact they keep. Use it for results the user should get, such as a report, a chart or cleaned data. Do not use it for scratch files. Unless the run has full access, the user approves the exact content first, and a file changed after that approval is refused. Regular files only, at most 64 MiB. Returns the id the host stored the file under.",
        schema,
        examples,
    )
}

fn get_status() -> Parts {
    let schema = closed(json!({}), &[]);
    (
        "Show the state of your sandbox: image, operating system, working folder, disk used and quota, allowed network hosts, pending host requests, and whether a command is running and who started it. Use it after an unknown outcome or a refused host, before deciding what to do next. Do not use it to read files; use sandbox_read_file instead. Returns a short report.",
        schema,
        vec![json!({})],
    )
}
