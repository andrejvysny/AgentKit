//! Conformance runner (TU-36): task format, checkers, runner and matrix report.
//! Tasks run against a fake workspace, so a run needs no tools, files or network
//! unless a live endpoint is explicitly authorised (see [`live_model`]).
//!
//! That runner and [`live_model`] test a substitute interface (string-only
//! arguments, generic descriptions, its own loop) and are NOT qualification
//! evidence. Qualification runs the production pipeline: [`production_scenarios`]
//! and [`Scenario::judge`] are the scenario and assertion library, and
//! [`QualificationRecord`] is the versioned result. The driver that runs them
//! against the real registries, profile, dialect and run loop lives in
//! `agentkit-runtime` (`tests/tool_use_qualification.rs`, layer 5), because the
//! run loop is above this crate; see `docs/qualification/tool-use-qualification.md`.
mod check;
pub mod cli;
mod live;
mod matrix;
mod mock;
mod qualification;
mod runner;
mod scenario;
mod scenarios;
mod task;

pub use check::{CallRecord, class_name, is_invalid};
pub use live::{LiveConfig, LiveModel, live_model};
pub use matrix::{Cell, Matrix, Thresholds, build_matrix};
pub use mock::{MockMode, MockModel};
pub use qualification::{
    QUALIFICATION_RECORD_VERSION, QualificationKey, QualificationRecord, ScenarioResult,
    ToolsetResult, digest_json,
};
pub use runner::{Model, ModelCall, ModelTurn, RunCtx, RunOutcome, run_matrix, run_task};
pub use scenario::{
    Checks, Observed, ObservedCall, ObservedMemory, Scenario, Script, ScriptedCall, StateCheck,
    Variant, Verdict,
};
pub use scenarios::production_scenarios;
pub use task::{
    Effect, Expect, ExpectedCall, MockScript, Predicate, Task, ToolSpec, load_dir, parse_task_file,
};
