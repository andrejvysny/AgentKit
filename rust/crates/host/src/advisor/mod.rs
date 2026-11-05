//! Advisor & Watchdog: read-only advisory review over the SAME execution
//! engine.
//!
//! - The **Advisor** runs a bounded, read-only tool loop with its own
//!   independently configurable model, and returns STRUCTURED,
//!   evidence-backed findings that are persisted durably
//!   (`advisor_findings`). Explicit reviews are requested by the main agent
//!   (tool) or the host (native API).
//! - The **Watchdog** observes durable lifecycle signals (settled tasks with
//!   failures, repeated tool failures counted from the child/turn event
//!   log) and triggers automatic reviews under strict loop and budget
//!   guards: reviews never trigger reviews, per-chat cooldowns bound
//!   frequency, and a rolling window caps model spend.
//!
//! Escalation: critical findings are (a) persisted with `escalated`
//! bookkeeping, (b) announced on the parent session's durable event stream
//! via the host event emitter, and (c) — for `critical` — injected into the
//! parent session as a durable steer input so the main agent must confront
//! them at its next tool boundary.

mod config;
mod engine;
mod tools;
mod watchdog;

pub use config::{AdvisorConfig, FindingCategory, FindingSeverity, ReviewRequest};
pub use engine::{
    AdvisorEngine, AdvisorEventSink, AdvisorReport, AdvisorToolSource, Finding, ReviewOrigin,
    ReviewReservation, StaticToolSource, advisor_run_chat_id, is_advisor_chat_id,
};
pub use tools::{ADVISOR_TOOL_NAMES, AdvisorContributor, AdvisorTools};
pub use watchdog::{Watchdog, WatchdogRunHooks, WatchdogSignals, explicit_review};
