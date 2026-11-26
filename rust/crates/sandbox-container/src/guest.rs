//! The guest requests of one machine, each bounded by the request timeout
//! and checked against the book and [`crate::verify`] before it reaches
//! the port.

use agentkit_sandbox_proto::guest::{ListRequest, ReadRequest, Reply, Request};
use agentkit_sandbox_proto::valid_operation_id;
use agentkit_workspaces::{
    COPY_IN_MAX_BYTES, DispatchAck, EXPORT_FILE_MAX_BYTES, FileChunk, LIST_DEPTH_MAX,
    LIST_DEPTH_MIN, LIST_MAX_ENTRIES, ListEntries, Listing, OBSERVE_CHUNK_MAX_BYTES, Observation,
    OperationId, OperationKind, OperationRequest, OperationState, ReadFile, SandboxError,
};

use crate::channel::Response;
use crate::expect::Expect;
use crate::machine::{Machine, OVERDUE_CANCEL_WAIT};
use crate::mapping;
use crate::verify::{self, Broken, ReadAsked};

/// Most output bytes one observe asks for (INV-9).
pub(crate) const OBSERVE_MAX_BYTES: u64 = OBSERVE_CHUNK_MAX_BYTES;
/// Most bytes one read asks for: the guest's own read bound.
const READ_MAX_BYTES: u64 = EXPORT_FILE_MAX_BYTES;

fn unknown(after_seq: u64) -> Observation {
    Observation {
        state: OperationState::Unknown,
        output: Vec::new(),
        next_seq: after_seq,
        truncated: false,
        eof: false,
    }
}

impl Machine {
    async fn call(
        &self,
        body: &Request,
        data: Option<Vec<u8>>,
        expect: Expect,
        what: &str,
    ) -> Result<Response, SandboxError> {
        self.channel
            .request(body, data, expect, self.request_timeout)
            .await
            .map_err(|error| self.channel_error(error, what))
    }

    /// `exec` or `write`. The operation is marked current for egress
    /// before it is sent, so a refusal it causes at once names it.
    pub(crate) async fn dispatch(
        &self,
        op: &OperationRequest,
    ) -> Result<DispatchAck, SandboxError> {
        let id = op.id.as_str();
        if !valid_operation_id(id) {
            return Err(SandboxError::Backend {
                reason: format!("operation id {id} cannot be sent to the guest"),
            });
        }
        let (request, data) = mapping::dispatch_request(op);
        if data
            .as_ref()
            .is_some_and(|bytes| bytes.len() as u64 > COPY_IN_MAX_BYTES)
        {
            return Err(SandboxError::FileTooLarge {
                size: data.map_or(0, |bytes| bytes.len() as u64),
                limit: COPY_IN_MAX_BYTES,
            });
        }
        let previous = self.mark_current(id);
        let dispatched_ms = self.now_ms();
        // On a lost answer the operation may run: the mark stays.
        let response = self
            .call(&request, data, Expect::Dispatch, "dispatch")
            .await?;
        match response.reply {
            Reply::Accepted { state, existing } => {
                let state = mapping::state(state);
                self.record_ack(id, state, existing)
                    .map_err(|broken| self.violation(broken))?;
                if state.is_terminal() {
                    // A replay of a settled operation started nothing.
                    self.restore_current(id, previous);
                } else if let OperationKind::Exec { timeout_ms, .. } = &op.kind {
                    self.set_deadline(id, dispatched_ms, *timeout_ms);
                }
                Ok(DispatchAck { state, existing })
            }
            Reply::Conflict => {
                self.restore_current(id, previous);
                Err(SandboxError::OperationConflict(op.id.clone()))
            }
            Reply::Error(error) => {
                self.restore_current(id, previous);
                Err(mapping::guest_failure("the dispatch", &error))
            }
            _ => Err(self.violation("a reply of the wrong kind")),
        }
    }

    /// `output` after `after_seq`, at most [`OBSERVE_MAX_BYTES`]. An exec
    /// still reported running past its host deadline is cancelled and the
    /// machine made unreachable ([`Self::past_deadline`]).
    pub(crate) async fn observe(
        &self,
        id: &OperationId,
        after_seq: u64,
    ) -> Result<Observation, SandboxError> {
        let op = id.as_str();
        if !valid_operation_id(op) {
            // The guest can hold no operation under this id.
            return Ok(unknown(after_seq));
        }
        let request = Request::Output {
            op: op.to_owned(),
            after_seq,
            max_bytes: OBSERVE_MAX_BYTES,
        };
        let expect = Expect::Output {
            max: OBSERVE_MAX_BYTES,
        };
        let response = self.call(&request, None, expect, "observe").await?;
        let header = match response.reply {
            Reply::Unknown => {
                self.record_state(op, OperationState::Unknown)
                    .map_err(|broken| self.violation(broken))?;
                return Ok(unknown(after_seq));
            }
            Reply::OutputData(header) => header,
            Reply::Error(error) => return Err(mapping::guest_failure("observe", &error)),
            _ => return Err(self.violation("a reply of the wrong kind")),
        };
        verify::output(after_seq, OBSERVE_MAX_BYTES, &header)
            .map_err(|broken| self.violation(broken))?;
        let state = mapping::state(header.state);
        self.record_state(op, state)
            .map_err(|broken| self.violation(broken))?;
        if state.is_terminal() {
            self.settled(op);
        } else if self.overdue(op) {
            return Err(self.past_deadline(op).await);
        }
        Ok(Observation {
            state,
            output: response.bytes,
            next_seq: header.next_seq,
            truncated: header.truncated,
            eof: header.eof,
        })
    }

    /// An exec the guest still reports running after its timeout and the
    /// grace: ask it to cancel (best effort, its answer awaited at most
    /// [`OVERDUE_CANCEL_WAIT`]), then close the channel as a protocol
    /// violation, also when the caller stops waiting during the cancel. The
    /// service's observe fails, so it makes the operation uncertain and
    /// stops the machine instead of waiting on the guest's word for ever.
    async fn past_deadline(&self, op: &str) -> SandboxError {
        const OVERDUE: Broken = "an exec was still reported running past its timeout and grace";
        let closing = self.close_on_drop(OVERDUE);
        let cancel = Request::Cancel { op: op.to_owned() };
        let wait = self.request_timeout.min(OVERDUE_CANCEL_WAIT);
        let _ = self
            .channel
            .request(&cancel, None, Expect::Cancel, wait)
            .await;
        drop(closing);
        self.violation(OVERDUE)
    }

    /// `cancel`. An id the guest does not know is left alone.
    pub(crate) async fn cancel(&self, id: &OperationId) -> Result<(), SandboxError> {
        let op = id.as_str();
        if !valid_operation_id(op) {
            return Ok(());
        }
        let request = Request::Cancel { op: op.to_owned() };
        let response = self.call(&request, None, Expect::Cancel, "cancel").await?;
        let state = match response.reply {
            Reply::State { state } => mapping::state(state),
            Reply::Unknown => OperationState::Unknown,
            Reply::Error(error) => return Err(mapping::guest_failure("cancel", &error)),
            _ => return Err(self.violation("a reply of the wrong kind")),
        };
        self.record_state(op, state)
            .map_err(|broken| self.violation(broken))
    }

    /// `read`, at most the request's bytes and never more than the guest's
    /// own read bound.
    pub(crate) async fn read(&self, req: &ReadFile) -> Result<FileChunk, SandboxError> {
        let limit = req.limit_bytes.min(READ_MAX_BYTES);
        let request = Request::Read(ReadRequest {
            root: mapping::root(req.root),
            path: req.path.as_str().to_owned(),
            offset: req.offset,
            limit_bytes: limit,
            max_file_bytes: req.max_file_bytes,
            raw: req.raw,
        });
        let response = self
            .call(&request, None, Expect::Read { max: limit }, "read")
            .await?;
        let header = match response.reply {
            Reply::FileData(header) => header,
            Reply::Error(error) => {
                return Err(mapping::file_error(
                    &error,
                    req.root,
                    &req.path,
                    req.max_file_bytes,
                ));
            }
            _ => return Err(self.violation("a reply of the wrong kind")),
        };
        let asked = ReadAsked {
            offset: req.offset,
            max_file_bytes: req.max_file_bytes,
            raw: req.raw,
        };
        verify::file(asked, &header).map_err(|broken| self.violation(broken))?;
        Ok(FileChunk {
            size: header.size,
            mtime_ms: header.mtime_ms,
            binary: header.binary,
            bytes: response.bytes,
            eof: header.eof,
        })
    }

    /// `list`, at most the request's entries and never more than
    /// [`LIST_MAX_ENTRIES`].
    pub(crate) async fn list(&self, req: &ListEntries) -> Result<Listing, SandboxError> {
        let depth = req.depth.clamp(LIST_DEPTH_MIN, LIST_DEPTH_MAX);
        let max_entries = req.max_entries.min(LIST_MAX_ENTRIES);
        let request = Request::List(ListRequest {
            root: mapping::root(req.root),
            path: req.path.as_str().to_owned(),
            depth,
            max_entries,
        });
        let response = self.call(&request, None, Expect::List, "list").await?;
        let (entries, truncated) = match response.reply {
            Reply::Entries { entries, truncated } => (entries, truncated),
            Reply::Error(error) => return Err(mapping::file_error(&error, req.root, &req.path, 0)),
            _ => return Err(self.violation("a reply of the wrong kind")),
        };
        let entries = verify::entries(&req.path, depth, max_entries, entries)
            .map_err(|broken| self.violation(broken))?;
        Ok(Listing { entries, truncated })
    }

    /// Bytes under `/workspace` and `/home/agent` together, or `None` when
    /// the guest could not measure everything (its figures are then not
    /// used at all).
    pub(crate) async fn usage(&self) -> Result<Option<u64>, SandboxError> {
        let response = self
            .call(&Request::Usage, None, Expect::Usage, "usage")
            .await?;
        match response.reply {
            Reply::Usage {
                workspace_bytes,
                home_bytes,
                complete,
            } => Ok(complete.then(|| workspace_bytes.saturating_add(home_bytes))),
            Reply::Error(error) => Err(mapping::guest_failure("usage", &error)),
            _ => Err(self.violation("a reply of the wrong kind")),
        }
    }

    /// `shutdown`, answered `bye` once every process ended.
    pub(crate) async fn shutdown_guest(&self) -> Result<(), SandboxError> {
        self.call(&Request::Shutdown, None, Expect::Shutdown, "shutdown")
            .await
            .map(|_| ())
    }
}
