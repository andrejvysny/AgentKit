//! Refused connections become network requests (NET-5). Bounded on every
//! side: one drain records at most `DENIALS_PER_OPERATION_MAX` distinct
//! targets, a workspace keeps at most `PENDING_REQUESTS_MAX` pending
//! requests, and an operation reports at most `DENIALS_PER_OPERATION_MAX`
//! targets, however many denials a backend returns.

use crate::ids::{OperationId, RequestId};
use crate::limits::{DENIALS_PER_OPERATION_MAX, PENDING_REQUESTS_MAX};
use crate::machine::MachineHandle;
use crate::network::{Denial, DenialRecord, HostPort, RequestState};

use super::WorkspaceService;

/// One distinct target of a batch, with its attempts.
struct Seen {
    target: HostPort,
    attempts: u64,
    last: Denial,
}

/// The distinct grantable targets of `denials`, at most the cap, and the
/// ones `op` tried, at most the cap.
fn distinct(denials: Vec<Denial>, op: &OperationId) -> (Vec<Seen>, Vec<HostPort>) {
    let mut seen: Vec<Seen> = Vec::new();
    let mut mine: Vec<HostPort> = Vec::new();
    for denial in denials {
        // A host that could never be granted is not put to the owner.
        let Ok(target) = HostPort::new(&denial.host, denial.port) else {
            continue;
        };
        let ours = denial.operation_id.as_ref() == Some(op);
        if ours && !mine.contains(&target) && mine.len() < DENIALS_PER_OPERATION_MAX {
            mine.push(target.clone());
        }
        if let Some(known) = seen.iter_mut().find(|known| known.target == target) {
            known.attempts += 1;
            known.last = denial;
        } else if seen.len() < DENIALS_PER_OPERATION_MAX {
            seen.push(Seen {
                target,
                attempts: 1,
                last: denial,
            });
        }
    }
    (seen, mine)
}

impl WorkspaceService {
    /// Record the machine's refused connections as network requests and
    /// answer the targets `op` tried. Best effort: a machine that cannot
    /// be drained reports none, and an unrecorded denial still reaches the
    /// caller.
    pub(super) async fn drain(
        &self,
        handle: Option<&MachineHandle>,
        op: &OperationId,
    ) -> Vec<HostPort> {
        let Some(handle) = handle else {
            return Vec::new();
        };
        let cursor = self.machines.denial_cursor(handle);
        let call = self.backend.drain_denials(handle, cursor);
        let Ok(batch) = self.bounded("drain_denials", call).await else {
            return Vec::new();
        };
        self.machines.set_denial_cursor(handle, batch.cursor);
        let (seen, mine) = distinct(batch.denials, op);
        if seen.is_empty() {
            return mine;
        }
        let ws = &handle.workspace_id;
        let Ok(pending) = self.repo.list_pending_requests(ws).await else {
            return mine;
        };
        let mut pending: Vec<HostPort> = pending.into_iter().map(|r| r.target).collect();
        for item in seen {
            let full = pending.len() >= PENDING_REQUESTS_MAX;
            if full && !pending.contains(&item.target) {
                continue;
            }
            let record = DenialRecord {
                id: RequestId::mint(&*self.runtime),
                workspace_id: ws.clone(),
                target: item.target.clone(),
                operation_id: item.last.operation_id,
                seen_at: item.last.at,
                attempts: item.attempts,
            };
            let recorded = self.repo.record_denial(&record).await;
            let now_pending = recorded.is_ok_and(|r| r.state == RequestState::Pending);
            if now_pending && !pending.contains(&item.target) {
                pending.push(item.target);
            }
        }
        mine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denial(host: &str, op: &str) -> Denial {
        Denial {
            host: host.to_owned(),
            port: 443,
            operation_id: Some(OperationId::new(op)),
            at: 1,
        }
    }

    #[test]
    fn a_flood_of_denials_is_bounded_and_counted() {
        let op = OperationId::new("op-1");
        let mut denials: Vec<Denial> = (0..200)
            .map(|n| denial(&format!("h{n}.example.com"), "op-1"))
            .collect();
        denials.push(denial("h0.example.com", "op-1"));
        denials.push(denial("not a host", "op-1"));
        let (seen, mine) = distinct(denials, &op);
        assert_eq!(seen.len(), DENIALS_PER_OPERATION_MAX);
        assert_eq!(mine.len(), DENIALS_PER_OPERATION_MAX);
        assert_eq!(seen[0].attempts, 2);
    }
}
