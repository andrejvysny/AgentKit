//! The JavaScript callback boundary.
//!
//! Rust never blocks the JS thread waiting for a callback: requests are
//! delivered through a threadsafe function, and the JS wrapper answers through
//! the exported resolver function. A callback lives exactly as long as one
//! in-flight call: its pending entry is removed by settle, rejection, abort,
//! or shutdown — never by GC. A rejected callback becomes the tool/provider
//! failure path (`callback_rejected`), never a panic and never a dangling
//! reference.
//!
//! The bridge owns an explicit `Open → Closing → Closed` lifecycle. Entering
//! `Closing` atomically rejects NEW dispatches and drains every outstanding
//! one under the same lock, so no call can be inserted after the drain and
//! then wait for JS that will never run again. That atomicity is why the
//! state lives inside the pending lock rather than in a separate flag.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use napi::bindgen_prelude::Env;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use serde_json::Value;
use tokio::sync::oneshot;

pub(crate) enum PendingKind {
    Provider,
    Tool,
    Retry,
    Steering,
    Budget,
    Hook,
    Settled,
}

struct Pending {
    sender: oneshot::Sender<std::result::Result<Value, String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BridgeState {
    Open,
    Closing,
    Closed,
}

struct Inner {
    state: BridgeState,
    pending: HashMap<String, Pending>,
}

/// Global routing so the exported resolver can find the owning bridge from a
/// bare call id. Entries exist only while a call is in flight.
static CALLS: LazyLock<Mutex<HashMap<String, Weak<JsBridge>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn calls() -> &'static Mutex<HashMap<String, Weak<JsBridge>>> {
    &CALLS
}

/// One bridge per session-service handle: one JS delivery function and the
/// outstanding provider/tool calls keyed by their call ids.
pub(crate) struct JsBridge {
    deliver: Mutex<ThreadsafeFunction<String>>,
    inner: Mutex<Inner>,
}

struct PendingGuard {
    bridge: Arc<JsBridge>,
    call_id: String,
    abort_on_drop: bool,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self
            .bridge
            .cancel(&self.call_id, "the callback future was dropped")
        {
            let kind = if self.abort_on_drop {
                "cancel"
            } else {
                "discard"
            };
            self.bridge.deliver_message(
                serde_json::json!({"kind":kind,"callId":self.call_id}).to_string(),
            );
        }
    }
}

impl JsBridge {
    pub(crate) fn new(deliver: ThreadsafeFunction<String>) -> Self {
        Self {
            deliver: Mutex::new(deliver),
            inner: Mutex::new(Inner {
                state: BridgeState::Open,
                pending: HashMap::new(),
            }),
        }
    }

    pub(crate) fn deliver_message(&self, text: String) -> napi::Status {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.state != BridgeState::Open {
            return napi::Status::Closing;
        }
        self.deliver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .call(Ok(text), ThreadsafeFunctionCallMode::NonBlocking)
    }

    /// Release the JS event-loop reference held by the delivery function.
    /// Called on close: a live service must keep the loop alive (turns run
    /// callbacks while JS awaits nothing), a closed one must not. The napi
    /// deprecation steers toward clone-for-refcounting; unref is exactly the
    /// uv-handle release this teardown needs.
    #[allow(deprecated)]
    pub(crate) fn detach(&self, env: &Env) -> std::result::Result<(), String> {
        self.deliver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unref(env)
            .map_err(|error| error.to_string())
    }

    /// Deliver a request to JS and await the JS resolver. Cancellation
    /// (worker stop, close, abort token) removes the pending entry so a late
    /// resolve is ignored instead of resurrecting a dead call. Once closing
    /// begins, NEW dispatches fail fast: an active run then sees an ordinary
    /// callback failure and settles honestly (receipts keep the uncertainty;
    /// nothing pretends success).
    pub(crate) async fn request(
        self: Arc<Self>,
        kind: PendingKind,
        call_id: &str,
        request: &Value,
        cancellation: &agentkit_core::CancellationToken,
    ) -> std::result::Result<Value, String> {
        let (sender, receiver) = oneshot::channel();
        let abort_on_drop = matches!(&kind, PendingKind::Tool);
        let label = match kind {
            PendingKind::Provider => "provider",
            PendingKind::Tool => "tool",
            PendingKind::Retry => "retry",
            PendingKind::Steering => "steering",
            PendingKind::Budget => "budget",
            PendingKind::Hook => "hook",
            PendingKind::Settled => "settled",
        };
        let payload = serde_json::json!({
            "kind": label,
            "callId": call_id,
            "request": request,
        });
        let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_owned());
        let status = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if inner.state != BridgeState::Open {
                return Err(format!(
                    "the {label} callback was not dispatched: the binding is closing"
                ));
            }
            inner.pending.insert(call_id.to_owned(), Pending { sender });
            calls()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(call_id.to_owned(), Arc::downgrade(&self));
            self.deliver
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .call(Ok(text), ThreadsafeFunctionCallMode::NonBlocking)
        };
        if status != napi::Status::Ok {
            self.cancel(call_id, "the JS delivery queue is closing");
            return Err("the JS delivery queue is closing".to_owned());
        }
        let _guard = PendingGuard {
            bridge: self.clone(),
            call_id: call_id.to_owned(),
            abort_on_drop,
        };
        tokio::select! {
            result = receiver => match result {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(message)) => Err(message),
                Err(_) => Err(format!("{label} callback was dropped before settling")),
            },
            _ = cancellation.cancelled() => {
                self.cancel(call_id, "the call was cancelled by the host");
                Err(format!("the {label} call was cancelled"))
            }
        }
    }

    /// The JS resolver settles exactly one call. A second settle for the same
    /// id (a wrapper bug or a replay) is ignored — the first answer wins.
    pub(crate) fn settle(&self, call_id: &str, result: std::result::Result<Value, String>) {
        if let Some(pending) = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending
            .remove(call_id)
        {
            let _ = pending.sender.send(result);
            calls()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(call_id);
        }
    }

    fn cancel(&self, call_id: &str, reason: &str) -> bool {
        if let Some(pending) = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending
            .remove(call_id)
        {
            let _ = pending.sender.send(Err(reason.to_owned()));
            calls()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(call_id);
            true
        } else {
            false
        }
    }

    /// `Open → Closing`, atomically with the drain: entering `Closing`
    /// rejects every outstanding callback and makes further dispatches
    /// fail fast under the same lock, so there is no insert-after-drain
    /// window. Returns false when the bridge was already closing.
    pub(crate) fn start_closing(&self) -> bool {
        let drained: Vec<(String, Pending)> = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if inner.state != BridgeState::Open {
                return false;
            }
            inner.state = BridgeState::Closing;
            inner.pending.drain().collect()
        };
        for (id, pending) in drained {
            calls()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            let _ = pending.sender.send(Err(
                "the binding closed before the callback settled".to_owned()
            ));
        }
        true
    }

    /// `Closing → Closed`; no dispatch path can distinguish it from Closing,
    /// but teardown observability keeps the lifecycle honest.
    pub(crate) fn mark_closed(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .state = BridgeState::Closed;
    }
}

pub(crate) fn active(call_id: &str) -> bool {
    calls()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(call_id)
}

/// Exported resolver entry point: JS answers exactly one in-flight callback.
/// `payload` is the JSON the callback produced; `error` is a rejection
/// message (mutually exclusive with payload). Returns false for unknown or
/// already-settled ids — never an exception for a stale resolve.
pub(crate) fn resolve(call_id: &str, payload: Option<String>, error: Option<String>) -> bool {
    let bridge = calls()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(call_id)
        .and_then(|weak| weak.upgrade());
    let Some(bridge) = bridge else {
        return false;
    };
    let result = match (error, payload) {
        (Some(message), _) => Err(message),
        (None, Some(text)) => serde_json::from_str::<Value>(&text)
            .map_err(|parse| format!("the callback produced invalid JSON: {parse}")),
        (None, None) => Err("the callback settled with neither result nor error".to_owned()),
    };
    bridge.settle(call_id, result);
    true
}
