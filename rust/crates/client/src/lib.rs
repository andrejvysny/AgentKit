//! Optional asynchronous HTTP client and native run state reducer.
mod error;
mod http;
mod phase;
mod presentation;
mod sse;
mod stream;

pub use agentkit_contracts::RestOperation;
pub use error::ClientError;
pub use http::{Call, Client};
pub use phase::{RunPhase, RunState};
pub use presentation::*;
pub use sse::{SseDecoder, SseFrame};
pub use stream::StreamOptions;
