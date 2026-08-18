import type { TaskRunner } from "./ports/task-runner.js";
import type { Logger } from "./ports/system.js";
import type { ProposalService } from "./proposals/proposal-service.js";

export interface RecoverOnBootDeps {
  taskRunner: TaskRunner;
  proposals: ProposalService;
  logger?: Logger;
}

export interface RecoverOnBootResult {
  /** Proposals a previous process left in `applying`, now settled either way. */
  proposalsReconciled: number;
}

/**
 * The startup pass a host runs BEFORE it starts claiming work.
 *
 * A crash leaves two kinds of debris, in two different subsystems, and neither
 * one can clean up the other:
 *
 * 1. **Runs** whose lease died with the process — `TaskRunner.recover()` expires
 *    those leases, ends their attempts `abandoned`, and re-enqueues or
 *    dead-letters each run.
 * 2. **Proposals stuck in `applying`** — a write that was claimed and may or may
 *    not have landed. `ProposalService.reconcileInterrupted()` settles each one
 *    by asking the applier what actually happened.
 *
 * Order matters: the queue's recovery runs first, so the worker that picks a
 * recovered run back up finds every proposal already resolved rather than
 * racing a reconciliation for the same scope. Both steps are idempotent, so a
 * host may call this on every boot without checking whether the last shutdown
 * was clean.
 *
 * Deliberately NOT started for you: this only cleans up. Call
 * `TaskRunner.startWorker` afterwards, when the host is ready to execute.
 */
export async function recoverOnBoot(
  deps: RecoverOnBootDeps,
): Promise<RecoverOnBootResult> {
  await deps.taskRunner.recover();
  const report = await deps.proposals.reconcileInterrupted();
  deps.logger?.info("recovered on boot", {
    proposalsReconciled: report.reconciled,
    proposalsApplied: report.applied,
    proposalsFailed: report.failed,
  });
  return { proposalsReconciled: report.reconciled };
}
