use serde::de::DeserializeOwned;
use serde_json::json;

use super::*;
use crate::frame::{Frame, FrameDecoder, encode_control, parse_control};

/// Encode `value` as a control frame, decode it and parse it back.
fn through_wire<T: Serialize + DeserializeOwned>(value: &T) -> T {
    let mut decoder = FrameDecoder::new();
    decoder.push(&encode_control(value).unwrap());
    let Some(Frame::Control(json)) = decoder.next_frame().unwrap() else {
        panic!("expected one control frame");
    };
    parse_control(&json).unwrap()
}

fn digest() -> String {
    "ab".repeat(32)
}

fn exec_request() -> ExecRequest {
    ExecRequest {
        op: "op-1".into(),
        args_sha256: digest(),
        command: "echo \"hi\" | tr a-z A-Z\n".into(),
        cwd: "chats/c1".into(),
        timeout_ms: 120_000,
        output_limit: OutputLimit {
            max_bytes: 64 << 20,
            max_lines: None,
        },
        tz: Some("Europe/Bratislava".into()),
    }
}

fn requests() -> Vec<Request> {
    vec![
        Request::Hello {
            protocol: crate::PROTOCOL_VERSION,
            generation: u64::MAX,
        },
        Request::Exec(exec_request()),
        Request::Write(WriteRequest {
            op: "op-2".into(),
            args_sha256: digest(),
            root: PathRoot::Home,
            path: ".config/x.toml".into(),
            sha256: digest(),
            size: 5,
            mode: 0o755,
        }),
        Request::Status { op: "op-1".into() },
        Request::Output {
            op: "op-1".into(),
            after_seq: 4096,
            max_bytes: 51_200,
        },
        Request::Cancel { op: "op-1".into() },
        Request::Read(ReadRequest {
            root: PathRoot::Workspace,
            path: "data.csv".into(),
            offset: 10,
            limit_bytes: 50,
            max_file_bytes: 10 << 20,
            raw: true,
        }),
        Request::List(ListRequest {
            root: PathRoot::Workspace,
            path: String::new(),
            depth: 4,
            max_entries: 500,
        }),
        Request::Usage,
        Request::Shutdown,
    ]
}

fn states() -> Vec<OperationState> {
    vec![
        OperationState::Accepted,
        OperationState::Running,
        OperationState::Succeeded { exit: 0 },
        OperationState::Failed {
            exit: Some(2),
            signal: None,
        },
        OperationState::Failed {
            exit: None,
            signal: Some(9),
        },
        OperationState::TimedOut,
        OperationState::Cancelled,
        OperationState::OutputLimit,
        OperationState::Unknown,
    ]
}

fn error_codes() -> Vec<GuestErrorCode> {
    vec![
        GuestErrorCode::NotFound,
        GuestErrorCode::NotADirectory,
        GuestErrorCode::NotARegularFile,
        GuestErrorCode::FileTooLarge { size: u64::MAX },
        GuestErrorCode::DigestMismatch,
        GuestErrorCode::SizeMismatch,
        GuestErrorCode::InvalidPath,
        GuestErrorCode::InvalidRequest,
        GuestErrorCode::ProtocolMismatch,
        GuestErrorCode::GenerationMismatch,
        GuestErrorCode::NoSpace,
        GuestErrorCode::Busy,
        GuestErrorCode::Internal,
    ]
}

fn entries() -> Vec<Entry> {
    let entry = |path: &str, kind, size| Entry {
        path: path.into(),
        kind,
        size,
    };
    vec![
        entry("a", EntryKind::Directory, None),
        entry("a/b.txt", EntryKind::File, Some(12)),
        entry("a/l", EntryKind::Symlink, None),
        entry("a/p", EntryKind::Other, None),
    ]
}

fn replies() -> Vec<Reply> {
    let mut replies = vec![
        Reply::Hello {
            protocol: 1,
            generation: 3,
            image_digest: format!("sha256:{}", digest()),
            channel_token: "t0k3n".into(),
        },
        Reply::Conflict,
        Reply::Unknown,
        Reply::FileData(FileData {
            size: 9,
            mtime_ms: -1,
            binary: true,
            eof: false,
            len: 0,
        }),
        Reply::Entries {
            entries: entries(),
            truncated: true,
        },
        Reply::Usage {
            workspace_bytes: u64::MAX,
            home_bytes: 0,
            complete: false,
        },
        Reply::Bye,
    ];
    for state in states() {
        replies.push(Reply::Accepted {
            state,
            existing: true,
        });
        replies.push(Reply::State { state });
        replies.push(Reply::OutputData(OutputData {
            state,
            next_seq: 8192,
            truncated: false,
            eof: true,
            len: 17,
        }));
    }
    for code in error_codes() {
        replies.push(Reply::Error(GuestError {
            code,
            message: "why".into(),
        }));
    }
    replies
}

#[test]
fn every_request_round_trips() {
    for (id, body) in requests().into_iter().enumerate() {
        let envelope = RequestEnvelope::new(id as u64, body);
        assert_eq!(through_wire(&envelope), envelope);
    }
}

#[test]
fn every_reply_round_trips() {
    for body in replies() {
        let envelope = ReplyEnvelope::new(u64::MAX, body);
        assert_eq!(through_wire(&envelope), envelope);
    }
}

#[test]
fn wire_shapes_are_tagged_snake_case() {
    let exec = serde_json::to_value(RequestEnvelope::new(1, requests().remove(1))).unwrap();
    assert_eq!(exec["body"]["type"], "exec");
    assert_eq!(exec["body"]["output_limit"]["max_lines"], json!(null));
    assert_eq!(exec["body"]["timeout_ms"], 120_000);
    let usage = serde_json::to_value(Request::Usage).unwrap();
    assert_eq!(usage, json!({"type": "usage"}));
    let state = Reply::State {
        state: OperationState::Failed {
            exit: None,
            signal: Some(9),
        },
    };
    assert_eq!(
        serde_json::to_value(state).unwrap(),
        json!({"type": "state", "state": {"kind": "failed", "exit": null, "signal": 9}})
    );
    let read = Request::Read(ReadRequest {
        root: PathRoot::Home,
        path: "a".into(),
        offset: 0,
        limit_bytes: 1,
        max_file_bytes: 2,
        raw: false,
    });
    assert_eq!(serde_json::to_value(read).unwrap()["root"], "home");
}

#[test]
fn error_codes_sit_beside_the_message() {
    let error = Reply::Error(GuestError {
        code: GuestErrorCode::FileTooLarge { size: 11 },
        message: "big".into(),
    });
    assert_eq!(
        serde_json::to_value(error).unwrap(),
        json!({"type": "error", "code": "file_too_large", "size": 11, "message": "big"})
    );
    let busy: Reply =
        serde_json::from_value(json!({"type": "error", "code": "busy", "message": ""})).unwrap();
    assert_eq!(
        busy,
        Reply::Error(GuestError {
            code: GuestErrorCode::Busy,
            message: String::new()
        })
    );
}

#[test]
fn refuses_unknown_types_codes_and_bad_numbers() {
    for bad in [
        json!({"type": "format_disk"}),
        json!({"type": "status"}),
        json!({"type": "hello", "protocol": -1, "generation": 1}),
        json!({"type": "hello", "protocol": 1, "generation": 1.5}),
        json!({"type": "write", "op": "o", "args_sha256": "", "root": "etc",
               "path": "p", "sha256": "", "size": 1, "mode": 420}),
    ] {
        assert!(
            serde_json::from_value::<Request>(bad.clone()).is_err(),
            "{bad}"
        );
    }
    for bad in [
        json!({"type": "error", "code": "teapot", "message": ""}),
        json!({"type": "error", "code": "file_too_large", "message": ""}),
        json!({"type": "state", "state": {"kind": "exploded"}}),
    ] {
        assert!(
            serde_json::from_value::<Reply>(bad.clone()).is_err(),
            "{bad}"
        );
    }
}
