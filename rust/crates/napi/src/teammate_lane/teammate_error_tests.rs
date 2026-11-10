use super::*;
use agentkit_sessions::SessionsError;
use agentkit_store::StoreError;
use serde_json::json;

fn mapped(error: impl Into<AgentsError>) -> Value {
    serde_json::from_str(&map_host(error).reason).unwrap()
}

#[test]
fn teammate_refusals_keep_wire_codes_and_messages() {
    for (error, message) in [
        (
            SessionsError::InvalidOverrides("bad model".into()),
            "invalid_overrides: bad model",
        ),
        (
            SessionsError::RunProfileRefused(" missing".into()),
            "run_profile_refused: missing",
        ),
    ] {
        assert_eq!(
            mapped(error),
            json!({"code": "invalid_input", "message": message})
        );
    }
    for (error, code, message) in [
        (
            StoreError::Conflict("rev 2".into()),
            "revision_conflict",
            "rev 2",
        ),
        (
            StoreError::NotFound("task missing".into()),
            "not_found",
            "record not found: task missing",
        ),
    ] {
        assert_eq!(
            mapped(AgentsError::Store(error)),
            json!({"code": code, "message": message})
        );
    }
    assert_eq!(
        mapped(AgentsError::SubagentNotFound("a".into())),
        json!({"code": "not_found", "message": "subagent_not_found: a"})
    );
}

#[test]
fn teammate_opaque_diagnostics_cannot_spoof_refusals() {
    for message in [
        "invalid_overrides: secret",
        "run_profile_refused: secret",
        "secret invalid_overrides: token",
    ] {
        let error = AgentsError::Sessions(SessionsError::ContentAuthorityCallback(message.into()));
        assert_eq!(
            mapped(error),
            json!({"code": "internal_error", "message": errors::GENERIC_MESSAGE})
        );
    }
}
