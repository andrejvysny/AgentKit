import { CONTRACT_VERSION, type TaskEventEnvelope } from "@agentkit/contracts";
import type { Clock, IdGenerator } from "../ports/system.js";
import type { TaskStore } from "../ports/task-store.js";

/**
 * An event before the transport base fields are stamped on: the vocabulary's
 * `type` plus whatever that vocabulary carries (`data`, and anything else — the
 * envelope is open, and the store reads only `seq` and `eventId`).
 */
export type TaskEventDraft = { type: string } & Record<string, unknown>;

export interface TaskEventWriter {
  /** Stamp, append under the lease, and return what was written. */
  emit(draft: TaskEventDraft): Promise<TaskEventEnvelope>;
}

export interface TaskEventWriterDeps {
  tasks: TaskStore;
  taskId: string;
  attemptId: string;
  /** Proof of ownership; a stale token makes every emit throw `LeaseLostError`. */
  leaseToken: string;
  clock: Clock;
  ids: IdGenerator;
}

/**
 * A per-attempt event writer for host-side and non-chat task events.
 *
 * NOT FOR USE INSIDE A CHAT PASS. While `runChat` is streaming, core's
 * `createEventStamper` owns the numbering for that pass — it was handed
 * `firstSeq` and counts upward in memory. A second writer numbering from
 * `nextSeq` against the same log would interleave two counters into one stream:
 * both would read the same "next" value between appends, and the log would
 * either reject the collision (`SeqConflictError`, mid-turn) or, worse on a
 * store that renumbered, silently reorder what a client already received. Use
 * this BETWEEN passes (the host-originated warnings a turn emits around the
 * model call) and for executors of other kinds, which have no core stamper at
 * all.
 *
 * Calling `nextSeq` per emit is safe for exactly the same reason the rule above
 * exists: the lease serializes writers, so there is one emitter at a time, and
 * `appendEvents` rejects a non-monotonic `seq` as the backstop if that ever
 * stops being true.
 */
export function createTaskEventWriter(
  deps: TaskEventWriterDeps,
): TaskEventWriter {
  const { tasks, taskId, attemptId, leaseToken, clock, ids } = deps;
  return {
    async emit(draft: TaskEventDraft): Promise<TaskEventEnvelope> {
      // Spread first, stamp second: the base fields are this writer's to
      // assign, and a draft that carried its own `seq` must not win.
      const event: TaskEventEnvelope = {
        ...draft,
        seq: await tasks.nextSeq(taskId),
        eventId: ids.eventId(),
        timestamp: clock.nowIso(),
        contractVersion: CONTRACT_VERSION,
        attemptId,
      };
      await tasks.appendEvents(taskId, [event], { leaseToken });
      return event;
    },
  };
}
