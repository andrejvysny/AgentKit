import type { AiRunEvent } from "@agentkit/contracts";
import type { AiRunEventDraft } from "@agentkit/core";
import type { Clock, IdGenerator } from "../ports/system.js";
import type { TaskStore } from "../ports/task-store.js";
import {
  createTaskEventWriter,
  type TaskEventDraft,
} from "../tasks/task-event-writer.js";
import type {
  RunProjector,
  RunProjectionContext,
  RunProjectionState,
} from "./projection.js";

/** Stamp a draft onto the run's log and project it, in one call. */
export interface RunEventFeed {
  emit(draft: AiRunEventDraft): Promise<AiRunEvent>;
}

export interface RunEventFeedDeps {
  projector: RunProjector;
  ctx: RunProjectionContext;
  state: RunProjectionState;
  tasks: TaskStore;
  clock: Clock;
  ids: IdGenerator;
}

/**
 * The drafts-in convenience over {@link RunProjector}: stamp, append, project.
 *
 * For a host executor that produces events one at a time and has no stamper of
 * its own — a cloud-delegated turn mapping remote frames as they arrive. The
 * numbering is `createTaskEventWriter`'s, not a second copy of it: `seq` comes
 * from `TaskStore.nextSeq` per emit, which is correct precisely because the
 * lease serializes writers, so there is one emitter at a time.
 *
 * TWO THINGS IT IS NOT FOR.
 *
 * A pass that already has a stamper — anything driving `runChat`, or a host
 * mapping a whole recorded run at once — must keep using that stamper and
 * {@link RunProjector.project}. Two counters over one log is the failure
 * `createTaskEventWriter` documents: both read the same "next" value between
 * appends, and the log either rejects the collision or silently reorders what a
 * client already received.
 *
 * And it stamps `timestamp` from {@link RunEventFeedDeps.clock}, so a draft's
 * own timestamp is replaced. A host that must preserve an upstream event's time
 * stamps with core's `createEventStamper` (which does not) and calls
 * {@link RunProjector.project} itself.
 */
export function createRunEventFeed(deps: RunEventFeedDeps): RunEventFeed {
  const writer = createTaskEventWriter({
    tasks: deps.tasks,
    taskId: deps.ctx.task.taskId,
    attemptId: deps.ctx.attemptId,
    leaseToken: deps.ctx.leaseToken,
    clock: deps.clock,
    ids: deps.ids,
  });
  return {
    async emit(draft: AiRunEventDraft): Promise<AiRunEvent> {
      const event = (await writer.emit(
        draft as unknown as TaskEventDraft,
      )) as unknown as AiRunEvent;
      // `reflect`, not `project`: the writer has already appended it, and a
      // second append would collide on `seq`.
      await deps.projector.reflect(deps.ctx, deps.state, event);
      return event;
    },
  };
}
