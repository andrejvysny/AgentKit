//! Endpoint probe (TU-20): checks P1 to P18 against any OpenAI-compatible
//! chat-completions endpoint. Talks raw HTTP so server quirks (field names,
//! status codes, template handling) stay visible instead of being normalised
//! by a provider adapter. The API key lives in [`Secret`] and is scrubbed from
//! every string that reaches a report.
mod checks;
mod checks_more;
mod client;
mod report;

pub use checks::{CHECK_IDS, run_checks};
pub use client::{Endpoint, Secret};
pub use report::{CheckResult, Report, Status};
