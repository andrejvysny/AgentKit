//! Concurrent execution of consecutive read-only tool calls of one response.
//!
//! Only the tool bodies overlap. The guards run first, in call order; every
//! event, record, hook and message is still produced by the sequential loop
//! in `run.rs`, which consumes the results here in the original call order.

use super::State;
use crate::{
    SuspendSlot, ToolRegistry,
    call_pipeline::Prepared,
    permission::{PLAN_CAPABILITY, ToolClass, classify_capability},
    tool_execution::{self, ToolOutcome},
};
use futures_util::StreamExt;
use std::{collections::HashMap, ops::Range, sync::Arc};

/// Tool bodies running at once within one read batch.
pub(super) const PARALLEL_READ_LIMIT: usize = 4;

/// A tool body that already ran for the call at its index.
pub(super) struct Early {
    pub outcome: ToolOutcome,
    pub slot: Arc<SuspendSlot>,
    pub started_ms: i64,
    pub finished_ms: i64,
}

/// What the batch settled ahead of the sequential loop, by call index.
#[derive(Default)]
pub(super) struct BatchResults {
    /// The `beforeToolCall` verdict (`Some` = denied), evaluated in order.
    pub gates: HashMap<usize, Option<String>>,
    pub early: HashMap<usize, Early>,
}

impl State {
    /// A call that may share a batch: a read, judged by capability first,
    /// that is neither a plan tool, a meta tool nor a `tool_call` pass-through
    /// (their effect is the inner tool's, so they stay sequential).
    fn parallel_readable(&self, registry: &ToolRegistry, prepared: &Prepared) -> bool {
        if prepared.failure.is_some() || prepared.outer.is_some() {
            return false;
        }
        let Some(tool) = registry.get(&prepared.call.name) else {
            return false;
        };
        let definition = tool.definition();
        if registry.toolset_of(&prepared.call.name).as_deref() == Some(crate::META_TOOLSET) {
            return false;
        }
        match classify_capability(&definition.capability) {
            Some(ToolClass::Read) => definition.capability != PLAN_CAPABILITY,
            Some(ToolClass::Net) => definition.effect == "read",
            Some(_) => false,
            None => definition.effect == "read",
        }
    }

    /// End (exclusive) of the read batch starting at `start`, within `cap`;
    /// `start` itself when it is not a batch of two or more. A `refused`
    /// call ends the batch: the batch would run it before the refusal.
    pub(super) fn read_batch_end(
        &self,
        registry: &ToolRegistry,
        prepared: &[Prepared],
        refused: &[bool],
        start: usize,
        cap: usize,
    ) -> usize {
        if !self.options.parallel_read_tools {
            return start;
        }
        let limit = prepared.len().min(cap);
        let end = (start..limit)
            .take_while(|index| {
                !refused[*index] && self.parallel_readable(registry, &prepared[*index])
            })
            .last()
            .map_or(start, |last| last + 1);
        if end - start >= 2 { end } else { start }
    }

    /// Gate every call of the batch in order, then run the allowed bodies
    /// `PARALLEL_READ_LIMIT` at a time. A cancellation drops the bodies still
    /// running; the sequential loop then reports them as cancelled.
    pub(super) async fn run_read_batch(
        &mut self,
        registry: &ToolRegistry,
        prepared: &[Prepared],
        range: Range<usize>,
    ) -> Result<BatchResults, crate::CoreError> {
        let mut results = BatchResults::default();
        let mut jobs = Vec::new();
        for index in range {
            let call = &prepared[index].call;
            let gate = if self.options.hooks.is_some() {
                self.tool_gate(registry, call).await?
            } else {
                None
            };
            let denied = gate.is_some();
            results.gates.insert(index, gate);
            if self.options.cancellation.is_cancelled() {
                break;
            }
            if !denied {
                let slot = Arc::new(SuspendSlot::default());
                let context = self.tool_context(call, Some(slot.clone()));
                jobs.push((index, call, context, slot));
            }
        }
        let runtime = self.options.runtime.clone();
        let cancellation = self.options.cancellation.clone();
        let env = BodyEnv {
            runtime: runtime.clone(),
            budget: self.options.request.budget.clone(),
            close: self.options.tool_close.clone(),
            output: self.options.output.clone(),
            timeout: self.options.default_tool_timeout_ms,
        };
        let mut bodies: Vec<BoxedBody<'_>> = Vec::new();
        for (index, call, context, slot) in jobs {
            bodies.push(Box::pin(run_body(
                registry,
                (index, call, context, slot),
                env.clone(),
            )));
        }
        let mut stream = futures_util::stream::iter(bodies).buffered(PARALLEL_READ_LIMIT);
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                next = stream.next() => match next {
                    Some((index, early)) => { results.early.insert(index, early); }
                    None => break,
                },
            }
        }
        Ok(results)
    }
}

type BoxedBody<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = (usize, Early)> + Send + 'a>>;

/// What every body of a batch shares.
#[derive(Clone)]
struct BodyEnv {
    runtime: Arc<dyn crate::Runtime>,
    budget: Option<Arc<crate::ExecutionBudget>>,
    close: Option<tokio_util::sync::CancellationToken>,
    output: Option<Arc<crate::OutputGate>>,
    timeout: Option<u64>,
}

/// One tool body, timed on the run's clock.
async fn run_body(
    registry: &ToolRegistry,
    job: (
        usize,
        &crate::ToolCall,
        crate::ToolContext,
        Arc<SuspendSlot>,
    ),
    env: BodyEnv,
) -> (usize, Early) {
    let (index, call, context, slot) = job;
    let started_ms = env.runtime.now_ms();
    let outcome = tool_execution::execute(
        registry,
        call,
        context,
        env.timeout,
        env.budget,
        env.close,
        env.output,
    )
    .await;
    (
        index,
        Early {
            outcome,
            slot,
            started_ms,
            finished_ms: env.runtime.now_ms(),
        },
    )
}
