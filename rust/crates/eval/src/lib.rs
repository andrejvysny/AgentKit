//! Layer A of the evaluation plan: deterministic harness-correctness checks.
//!
//! No model, no network, no clock reads. The suites under `tests/` coordinate
//! infrastructure that already exists (testkit scripted providers and
//! fixtures, the provider conformance scenarios, the runtime qualification
//! driver, the context capture harness); this library holds only what no
//! single suite can own:
//!
//! - [`http_fixture`]: a transport that serves raw HTTP/SSE bytes from
//!   `fixtures/http/*.sse` to the real provider parsers;
//! - [`run`]: drives the real run loop and reduces its events to a
//!   [`run::Run`];
//! - [`oracle`]: a mini grader over a normalized trace with a three-valued
//!   outcome, so a grader bug cannot silently pass a bad run;
//! - [`coverage`]: the invariant to existing-test map, checked against the
//!   source tree so the README table cannot rot.
//!
//! Dev-only in spirit: nothing in the product depends on this crate.

pub mod coverage;
pub mod http_fixture;
pub mod oracle;
pub mod run;
