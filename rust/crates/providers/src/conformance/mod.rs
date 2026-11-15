//! Conformance runner (TU-36): task format, checkers, runner and matrix report.
//! Tasks run against a fake workspace, so a run needs no tools, files or network
//! unless a live endpoint is explicitly authorised (see [`live_model`]).
mod check;
pub mod cli;
mod live;
mod matrix;
mod mock;
mod runner;
mod task;

pub use check::{CallRecord, class_name, is_invalid};
pub use live::{LiveConfig, LiveModel, live_model};
pub use matrix::{Cell, Matrix, Thresholds, build_matrix};
pub use mock::{MockMode, MockModel};
pub use runner::{Model, ModelCall, ModelTurn, RunCtx, RunOutcome, run_matrix, run_task};
pub use task::{
    Effect, Expect, ExpectedCall, MockScript, Predicate, Task, ToolSpec, load_dir, parse_task_file,
};
