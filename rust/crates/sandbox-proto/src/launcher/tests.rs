use std::io::Cursor;

use serde_json::json;

use super::*;
use crate::frame::{Frame, parse_control, read_frame, write_frame};

fn through_wire<T: Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
    let mut wire = Vec::new();
    let frame = Frame::Control(serde_json::to_string(value).unwrap());
    write_frame(&mut wire, &frame).unwrap();
    let Some(Frame::Control(json)) = read_frame(&mut Cursor::new(wire)).unwrap() else {
        panic!("expected one control frame");
    };
    parse_control(&json).unwrap()
}

fn requests() -> Vec<Request> {
    let ws = || "019a7c3e-ws".to_string();
    vec![
        Request::Ensure {
            workspace_id: ws(),
            generation: 2,
            image_digest: "sha256:00".into(),
            tier: Tier {
                cpus_milli: 2000,
                memory_bytes: 2 << 30,
                pids: 512,
            },
        },
        Request::Stop {
            workspace_id: ws(),
            generation: u64::MAX,
            grace_ms: 5000,
        },
        Request::Inspect { workspace_id: ws() },
        Request::Usage { workspace_id: ws() },
        Request::ResetHome { workspace_id: ws() },
        Request::Destroy { workspace_id: ws() },
    ]
}

fn replies() -> Vec<Reply> {
    let mut replies = vec![
        Reply::Ensured {
            container_id: "c0ffee".into(),
            channel_token: "t0k3n".into(),
        },
        Reply::Stopped {
            proven: true,
            reason: None,
        },
        Reply::Stopped {
            proven: false,
            reason: Some("runtime unreachable".into()),
        },
        Reply::Inspected {
            running: Some(3),
            stopped_through: Some(2),
        },
        Reply::Inspected {
            running: None,
            stopped_through: None,
        },
        Reply::Usage {
            workspace_bytes: 1,
            home_bytes: u64::MAX,
        },
        Reply::Done,
    ];
    for code in [
        LauncherErrorCode::StaleGeneration { current: u64::MAX },
        LauncherErrorCode::Busy,
        LauncherErrorCode::NotFound,
        LauncherErrorCode::InvalidRequest,
        LauncherErrorCode::RuntimeFailed,
        LauncherErrorCode::ImageMismatch,
    ] {
        replies.push(Reply::Error(LauncherError {
            code,
            message: "m".into(),
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
        let envelope = ReplyEnvelope::new(7, body);
        assert_eq!(through_wire(&envelope), envelope);
    }
}

#[test]
fn wire_shapes_are_tagged_snake_case() {
    let reset = serde_json::to_value(RequestEnvelope::new(4, requests().remove(4))).unwrap();
    assert_eq!(
        reset,
        json!({"id": 4, "body": {"type": "reset_home", "workspace_id": "019a7c3e-ws"}})
    );
    let stale = Reply::Error(LauncherError {
        code: LauncherErrorCode::StaleGeneration { current: 4 },
        message: "old".into(),
    });
    assert_eq!(
        serde_json::to_value(stale).unwrap(),
        json!({"type": "error", "code": "stale_generation", "current": 4, "message": "old"})
    );
}

#[test]
fn refuses_requests_outside_the_closed_api() {
    for bad in [
        json!({"type": "run", "image": "evil"}),
        json!({"type": "ensure", "workspace_id": "w", "generation": 1, "image_digest": "d"}),
        json!({"type": "stop", "workspace_id": "w", "generation": -1, "grace_ms": 0}),
    ] {
        assert!(
            serde_json::from_value::<Request>(bad.clone()).is_err(),
            "{bad}"
        );
    }
}
