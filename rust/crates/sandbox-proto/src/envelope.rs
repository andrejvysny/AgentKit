//! The control envelope both protocols use.

use serde::{Deserialize, Serialize};

/// One control message: `{ "id": u64, "body": { "type": ..., ... } }`.
/// A reply carries its request's id, so several requests may be in flight
/// on one channel. Data frames that follow a message carry the same id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<B> {
    /// Request id, chosen by the requester; unique among its requests in
    /// flight.
    pub id: u64,
    /// The message.
    pub body: B,
}

impl<B> Envelope<B> {
    /// Wrap `body` with `id`.
    pub fn new(id: u64, body: B) -> Self {
        Self { id, body }
    }
}
