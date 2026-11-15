//! Child-wait identity (delegation 4.3): the reserved request shape a parked
//! parent's suspension carries. Lives in the engine so the store (which
//! mints it) and sessions (which must refuse it) share one definition
//! without sessions depending on the store.
use serde_json::Value;

/// Content `kind` of a child-wait park.
pub const CHILD_WAIT_KIND: &str = "child_wait";

/// Reserved request-id prefix of a child-wait park. Only the store's
/// `park_turn` may mint a record that carries it, so a content `kind` alone
/// never grants the no-slot park or the waker-only answer path.
pub const CHILD_WAIT_ID_PREFIX: &str = "childwait:";

/// Whether a request is a genuine child-wait park: the reserved id AND the
/// kind.
pub fn is_child_wait(request_id: &str, content: &Value) -> bool {
    request_id.starts_with(CHILD_WAIT_ID_PREFIX)
        && content.get("kind").and_then(Value::as_str) == Some(CHILD_WAIT_KIND)
}

/// Whether either half of the child-wait identity is claimed.
pub fn claims_child_wait(request_id: &str, content: &Value) -> bool {
    request_id.starts_with(CHILD_WAIT_ID_PREFIX)
        || content.get("kind").and_then(Value::as_str) == Some(CHILD_WAIT_KIND)
}
