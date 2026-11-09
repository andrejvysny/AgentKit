//! The advisor's escalation channel into the JS host.

use std::sync::Arc;

use agentkit_agents::AdvisorEventSink;

/// The bridge forwards `advisor.finding` escalations to the JS host as
/// ordinary service events (same channel, `kind` discriminator).
pub(crate) struct BridgeAdvisorSink {
    bridge: Arc<crate::jsbridge::JsBridge>,
}

impl BridgeAdvisorSink {
    pub(crate) fn new(bridge: Arc<crate::jsbridge::JsBridge>) -> Self {
        Self { bridge }
    }
}

impl AdvisorEventSink for BridgeAdvisorSink {
    fn emit(&self, event: serde_json::Value) {
        let status = self.bridge.deliver_message(event.to_string());
        if status != napi::Status::Ok {
            // An undeliverable ESCALATION must at least leave a trace: a
            // critical finding vanishing into a full/closing JS queue is
            // exactly the failure the event channel exists to prevent.
            eprintln!("advisor escalation dropped ({status:?}): {}", event);
        }
    }
}
