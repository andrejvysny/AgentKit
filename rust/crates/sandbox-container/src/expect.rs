//! What a guest request may be answered with. The reader checks every
//! reply against its request before it hands anything on: a reply of
//! another kind, or a data stream announced longer than the request
//! allows, breaks the protocol and closes the channel (ADR 0039: guest
//! replies are untrusted).

use agentkit_sandbox_proto::guest::Reply;

/// The replies one request allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expect {
    /// `hello`: `hello` or `error`.
    Hello,
    /// `exec` or `write`: `accepted`, `conflict` or `error`.
    Dispatch,
    /// `output`: `output_data` with at most `max` bytes, `unknown` or
    /// `error`.
    Output {
        /// Most bytes the data stream may carry.
        max: u64,
    },
    /// `cancel`: `state`, `unknown` or `error`.
    Cancel,
    /// `read`: `file_data` with at most `max` bytes, or `error`.
    Read {
        /// Most bytes the data stream may carry.
        max: u64,
    },
    /// `list`: `entries` or `error`.
    List,
    /// `usage`: `usage` or `error`.
    Usage,
    /// `shutdown`: `bye` or `error`.
    Shutdown,
}

/// What the reader does with a control reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The reply is the whole answer.
    Complete,
    /// The reply announces a data stream of this many bytes.
    Stream(u64),
    /// The reply breaks the protocol.
    Violation(&'static str),
}

impl Expect {
    /// Judge `reply` as the answer to this request.
    pub(crate) fn verdict(self, reply: &Reply) -> Verdict {
        if matches!(reply, Reply::Error(_)) {
            return Verdict::Complete;
        }
        match (self, reply) {
            (Expect::Hello, Reply::Hello { .. })
            | (Expect::Dispatch, Reply::Accepted { .. } | Reply::Conflict)
            | (Expect::Output { .. } | Expect::Cancel, Reply::Unknown)
            | (Expect::Cancel, Reply::State { .. })
            | (Expect::List, Reply::Entries { .. })
            | (Expect::Usage, Reply::Usage { .. })
            | (Expect::Shutdown, Reply::Bye) => Verdict::Complete,
            (Expect::Output { max }, Reply::OutputData(header)) => bounded(header.len, max),
            (Expect::Read { max }, Reply::FileData(header)) => bounded(header.len, max),
            _ => Verdict::Violation("a reply of the wrong kind"),
        }
    }
}

fn bounded(len: u64, max: u64) -> Verdict {
    if len > max {
        Verdict::Violation("a data stream longer than the request allows")
    } else {
        Verdict::Stream(len)
    }
}

#[cfg(test)]
mod tests {
    use agentkit_sandbox_proto::guest::{GuestError, GuestErrorCode, OperationState, OutputData};

    use super::*;

    fn output(len: u64) -> Reply {
        Reply::OutputData(OutputData {
            state: OperationState::Running,
            next_seq: len,
            truncated: false,
            eof: false,
            len,
        })
    }

    #[test]
    fn replies_are_judged_against_their_request() {
        let error = Reply::Error(GuestError {
            code: GuestErrorCode::Internal,
            message: String::new(),
        });
        assert_eq!(Expect::List.verdict(&error), Verdict::Complete);
        assert_eq!(
            Expect::Dispatch.verdict(&Reply::Conflict),
            Verdict::Complete
        );
        let output_max = Expect::Output { max: 10 };
        assert_eq!(output_max.verdict(&output(10)), Verdict::Stream(10));
        assert!(matches!(
            output_max.verdict(&output(11)),
            Verdict::Violation(_)
        ));
        assert!(matches!(
            Expect::Read { max: 10 }.verdict(&output(1)),
            Verdict::Violation(_)
        ));
        assert!(matches!(
            Expect::Dispatch.verdict(&Reply::Bye),
            Verdict::Violation(_)
        ));
    }
}
