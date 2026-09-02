/**
 * `bun:sqlite`-backed {@link TaskStore}: tasks, attempts, leases (with the
 * store-global fencing counter), task events, and the `claimNext` walk with
 * priority aging and dependency gating.
 *
 * Split out of `sqlite-assistant-store.ts` — one sub-store per file, sharing
 * {@link SqliteConnection} and the row mappers in `rows.js`.
 */
import type { TaskEventEnvelope } from "@agentkit/contracts";
import {
  assertScopeIdle,
  assertTaskTransition,
  ChatBusyError,
  DuplicateTaskError,
  evaluateTaskDependencies,
  InvalidTaskTransitionError,
  LeaseLostError,
  RecordNotFoundError,
  resolveTaskAging,
  SeqConflictError,
  TERMINAL_TASK_STATUSES,
  UnknownDependencyError,
  type AcquireLeaseInput,
  type AppendEventsOptions,
  type AttemptRecord,
  type ClaimedTask,
  type ClaimNextInput,
  type Clock,
  type CreateAttemptInput,
  type CreateTaskInput,
  type EndAttemptInput,
  type FencedWriteOptions,
  type IdGenerator,
  type Lease,
  type ListEventsOptions,
  type ResolvedTaskAging,
  type TaskAgingOptions,
  type TaskDependencyState,
  type TaskPatch,
  type TaskRecord,
  type TaskStatus,
  type TaskStore,
  type UpdateProgressOptions,
} from "@agentkit/host";
import type { Params, SqliteConnection, TxOwner } from "./connection.js";
import {
  type AttemptRow,
  attemptFromRow,
  isConstraintError,
  type LeaseRow,
  leaseFromRow,
  normalizeInstant,
  parseJson,
  type TaskEventRow,
  type TaskRow,
  taskFromRow,
  toJson,
} from "./rows.js";

const DEFAULT_LEASE_TTL_MS = 30_000;

/**
 * The statuses that make a scope undeletable — the same two
 * `ConversationService.deleteChat` refuses on, restated here because the STORE
 * owns the guarantee (see `TaskStore.deleteByScope`) and a store cannot import
 * a service's private constant.
 *
 * Typed as `TaskStatus[]` on purpose: a status renamed out of the union fails
 * to compile here rather than turning this guard into a filter that matches
 * nothing.
 */
const BUSY_TASK_STATUSES: readonly TaskStatus[] = Object.freeze([
  "running",
  "waiting_approval",
]);

/**
 * {@link BUSY_TASK_STATUSES} as an SQL `IN` list, built from the same constant
 * so the two cannot drift. Interpolated rather than bound because these are
 * this module's own compile-time literals, never caller input.
 */
const BUSY_TASK_STATUS_SQL = BUSY_TASK_STATUSES.map(
  (status) => `'${status}'`,
).join(", ");

/**
 * {@link TERMINAL_TASK_STATUSES} as an SQL `IN` list — what
 * {@link CreateTaskInput.exclusiveScope} negates to find the live rows.
 *
 * Built from the port's constant, which is itself derived from
 * `TASK_TRANSITIONS`, so "unfinished" here means exactly what it means
 * everywhere else and a new status cannot be forgotten in one of the two.
 * `NOT IN` rather than a hand-written live list for the same reason: adding a
 * non-terminal status must make it exclusive by default, not silently
 * claimable alongside a running turn.
 */
const TERMINAL_TASK_STATUS_SQL = TERMINAL_TASK_STATUSES.map(
  (status) => `'${status}'`,
).join(", ");

/**
 * Refuse a scope delete while anything in it is live, naming what is holding it.
 *
 * The message and `details` shape are deliberately byte-identical to the ones
 * `ConversationService.deleteChat` raises from its own fast-path check: a
 * caller (or a transport mapping `chat_busy` to a 409) must not be able to tell
 * which of the two layers refused.
 */
function assertScopeNotBusy(
  scopeId: string,
  busy: readonly { task_id: string; status: string }[],
): void {
  if (busy.length === 0) return;
  throw new ChatBusyError(
    `Chat ${scopeId} has ${busy.length} task(s) still running or awaiting approval; cancel or await them before deleting.`,
    {
      chatId: scopeId,
      taskIds: busy.map((row) => row.task_id),
      statuses: busy.map((row) => row.status),
    },
  );
}

export class SqliteTaskStore implements TaskStore {
  private readonly aging: ResolvedTaskAging;

  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
    private readonly leaseTtlMs: number = DEFAULT_LEASE_TTL_MS,
    aging: TaskAgingOptions = {},
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {
    this.aging = resolveTaskAging(aging);
  }

  async createTask(input: CreateTaskInput): Promise<TaskRecord> {
    const now = this.clock.nowIso();
    // Normalized before it is stored — `selectClaimCandidates` compares this
    // column as TEXT; see `normalizeInstant`.
    const availableAt =
      input.availableAt === undefined
        ? now
        : normalizeInstant(input.availableAt, "availableAt");
    const priority = input.priority ?? 0;
    // Immutable after create, so the array is copied out of the caller's hands
    // before it is serialized — and normalized to NULL when absent, which is
    // what `taskFromRow` reads back as "no gate".
    const dependsOn =
      input.dependsOn === undefined ? null : [...input.dependsOn];
    try {
      // Inside a transaction with the INSERT: the existence proof and the write
      // that relies on it must not be separated by another connection's commit,
      // or a concurrent delete between them would leave the dangling edge this
      // check exists to prevent.
      await this.conn.whenFree(() => {
        // EXCLUSIVITY IS THE FIRST THING IN THIS TRANSACTION, and the duplicate
        // check comes before it. Nothing runs between the statements of a
        // synchronous `whenFree` body, so the read and the INSERT below it are
        // atomic — which is the whole reason the refusal lives here and not in
        // the caller, where the two are separated by an await. The duplicate
        // check is explicit rather than left to the PK constraint because the
        // constraint fires AFTER the busy check, and a redelivery of a submit
        // whose task is still running must be answered as a duplicate, not
        // refused as busy. See `CreateTaskInput.exclusiveScope`.
        if (input.exclusiveScope === true) {
          if (this.selectTaskRow(input.taskId) !== null) {
            throw new DuplicateTaskError(
              `Task already exists: ${input.taskId}.`,
              { taskId: input.taskId },
            );
          }
          assertScopeIdle(
            input.scopeId,
            (
              this.conn.all(
                `SELECT task_id, status FROM tasks
                  WHERE scope_id = $scopeId AND status NOT IN (${TERMINAL_TASK_STATUS_SQL})
                  ORDER BY enqueued_at ASC, rowid ASC`,
                { $scopeId: input.scopeId },
              ) as { task_id: string; status: string }[]
            ).map((row) => ({ taskId: row.task_id, status: row.status })),
          );
        }
        this.assertDependenciesExist(
          input.taskId,
          input.parentTaskId,
          dependsOn,
        );
        this.conn.run(
          `INSERT INTO tasks
             (task_id, kind, scope_id, status, priority, enqueued_at, available_at, payload,
              parent_task_id, depends_on, attempt_count, poison_count)
           VALUES
             ($taskId, $kind, $scopeId, 'queued', $priority, $now, $availableAt, $payload,
              $parentTaskId, $dependsOn, 0, 0)`,
          {
            $taskId: input.taskId,
            $kind: input.kind,
            $scopeId: input.scopeId,
            $priority: priority,
            $now: now,
            $availableAt: availableAt,
            $payload: toJson(input.payload),
            $parentTaskId: input.parentTaskId ?? null,
            $dependsOn: dependsOn === null ? null : toJson(dependsOn),
          },
        );
      }, this.txOwner);
    } catch (err) {
      // The PK collision IS the idempotency guard doing its job; leaking the
      // raw SQLite constraint error would make every caller match on a driver
      // string to tell "already submitted" from "the database is broken".
      // `isConstraintError` does not check WHICH constraint tripped, which is
      // sound only while `task_id`'s primary key is the sole unique constraint
      // on `tasks` — see its doc comment before adding another unique index.
      if (isConstraintError(err)) {
        throw new DuplicateTaskError(`Task already exists: ${input.taskId}.`, {
          taskId: input.taskId,
          cause: String(err),
        });
      }
      throw err;
    }
    return {
      taskId: input.taskId,
      kind: input.kind,
      scopeId: input.scopeId,
      status: "queued",
      priority,
      enqueuedAt: now,
      availableAt,
      payload: input.payload,
      ...(input.parentTaskId === undefined
        ? {}
        : { parentTaskId: input.parentTaskId }),
      ...(dependsOn === null ? {} : { dependsOn }),
      attemptCount: 0,
      poisonCount: 0,
    };
  }

  async getTask(taskId: string): Promise<TaskRecord | null> {
    const row = this.selectTaskRow(taskId);
    return row ? taskFromRow(row) : null;
  }

  async listChildren(taskId: string): Promise<TaskRecord[]> {
    const rows = this.conn.all(
      `SELECT * FROM tasks WHERE parent_task_id = $parentTaskId ORDER BY enqueued_at ASC, rowid ASC`,
      { $parentTaskId: taskId },
    ) as TaskRow[];
    return rows.map(taskFromRow);
  }

  async listByScope(scopeId: string): Promise<TaskRecord[]> {
    const rows = this.conn.all(
      `SELECT * FROM tasks WHERE scope_id = $scopeId ORDER BY enqueued_at ASC, rowid ASC`,
      { $scopeId: scopeId },
    ) as TaskRow[];
    return rows.map(taskFromRow);
  }

  /**
   * Delete a scope's tasks with everything hanging off them, in ONE
   * transaction — unless something in the scope is still live, in which case
   * NOTHING is deleted and {@link ChatBusyError} is raised.
   *
   * THE BUSY CHECK IS THE FIRST STATEMENT OF THAT SAME TRANSACTION, and there
   * is no `await` between it and the deletes. That is the whole point of the
   * guard living here rather than only in `ConversationService.deleteChat`: the
   * service's check runs inside an async transaction, and a concurrent
   * `claimNext` on this connection FLATTENS into it (see
   * {@link SqliteConnection}) — so a task can go `queued → running` between the
   * service's check and this call. Nothing can run between statements of a
   * synchronous `withTx` body, so a check made here holds for the deletes that
   * follow it. See `TaskStore.deleteByScope`.
   *
   * Children before parents, because `task_attempts` and `leases` carry real
   * foreign keys to `tasks` and SQLite is not going to let a task row leave
   * while either still names it. `task_events` has no FK — it is keyed by
   * `task_id` alone, deliberately, so an event log outlives the attempt that
   * wrote it — which is exactly why it has to be deleted explicitly here rather
   * than swept up by a cascade that does not exist.
   */
  async deleteByScope(scopeId: string): Promise<number> {
    return this.conn.whenFree(() => {
      const scoped = `SELECT task_id FROM tasks WHERE scope_id = $scopeId`;
      const params: Params = { $scopeId: scopeId };
      // Same ordering as `listByScope`, so the ids and statuses this refusal
      // names are the ones the caller's own pre-check would have listed.
      assertScopeNotBusy(
        scopeId,
        this.conn.all(
          `SELECT task_id, status FROM tasks
            WHERE scope_id = $scopeId AND status IN (${BUSY_TASK_STATUS_SQL})
            ORDER BY enqueued_at ASC, rowid ASC`,
          params,
        ) as { task_id: string; status: string }[],
      );
      this.conn.run(
        `DELETE FROM task_events WHERE task_id IN (${scoped})`,
        params,
      );
      this.conn.run(`DELETE FROM leases WHERE task_id IN (${scoped})`, params);
      this.conn.run(
        `DELETE FROM task_attempts WHERE task_id IN (${scoped})`,
        params,
      );
      return this.conn.run(
        `DELETE FROM tasks WHERE scope_id = $scopeId`,
        params,
      ).changes;
    }, this.txOwner);
  }

  async transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord> {
    return this.transitionTaskAs(this.txOwner, taskId, from, to, patch, opts);
  }

  /**
   * {@link transitionTask}, told which transaction it belongs to.
   *
   * `claimNext` settles and claims candidates through this rather than through
   * the public method: it is already inside its own async transaction, and the
   * public method would gate on `this.txOwner` — undefined on the root store —
   * and wait for the very transaction it is running in.
   */
  private async transitionTaskAs(
    owner: TxOwner | undefined,
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord> {
    const availableAt =
      patch?.availableAt === undefined
        ? null
        : normalizeInstant(patch.availableAt, "availableAt");
    return this.conn.whenFree(() => {
      const row = this.selectTaskRow(taskId);
      if (!row) throw new RecordNotFoundError(`Task not found: ${taskId}`);
      if (opts?.leaseToken !== undefined) {
        this.assertLeaseCurrent(taskId, opts.leaseToken);
      }
      const current = row.status as TaskStatus;
      if (!from.includes(current)) {
        throw new InvalidTaskTransitionError(
          `Task ${taskId} is ${current}, expected one of [${from.join(", ")}].`,
          { taskId, current, from, to },
        );
      }
      assertTaskTransition(current, to);
      const result = this.conn.run(
        `UPDATE tasks SET
           status = $status,
           started_at = COALESCE($startedAt, started_at),
           finished_at = COALESCE($finishedAt, finished_at),
           error = COALESCE($error, error),
           available_at = COALESCE($availableAt, available_at),
           priority = COALESCE($priority, priority),
           poison_count = COALESCE($poisonCount, poison_count),
           payload = COALESCE($payload, payload)
         WHERE task_id = $id AND status = $current`,
        {
          $status: to,
          $startedAt: patch?.startedAt ?? null,
          $finishedAt: patch?.finishedAt ?? null,
          $error: patch?.error ?? null,
          $availableAt: availableAt,
          $priority: patch?.priority ?? null,
          $poisonCount: patch?.poisonCount ?? null,
          $payload: patch?.payload !== undefined ? toJson(patch.payload) : null,
          $id: taskId,
          $current: current,
        },
      );
      if (result.changes === 0) {
        // Lost a race between the SELECT above and this UPDATE.
        throw new InvalidTaskTransitionError(
          `Task ${taskId} changed concurrently; expected one of [${from.join(", ")}].`,
          { taskId, from, to },
        );
      }
      return taskFromRow(this.selectTaskRow(taskId)!);
    }, owner);
  }

  async createAttempt(input: CreateAttemptInput): Promise<AttemptRecord> {
    return this.createAttemptAs(this.txOwner, input);
  }

  /** {@link createAttempt}, told which transaction it belongs to. */
  private async createAttemptAs(
    owner: TxOwner | undefined,
    input: CreateAttemptInput,
  ): Promise<AttemptRecord> {
    return this.conn.whenFree(() => {
      const task = this.selectTaskRow(input.taskId);
      if (!task)
        throw new RecordNotFoundError(`Task not found: ${input.taskId}`);
      const attemptNumber = task.attempt_count + 1;
      const startedAt = this.clock.nowIso();
      this.conn.run(`UPDATE tasks SET attempt_count = $n WHERE task_id = $id`, {
        $n: attemptNumber,
        $id: input.taskId,
      });
      this.conn.run(
        `INSERT INTO task_attempts (attempt_id, task_id, attempt_number, status, owner_id, started_at)
         VALUES ($attemptId, $taskId, $attemptNumber, 'running', $ownerId, $startedAt)`,
        {
          $attemptId: input.attemptId,
          $taskId: input.taskId,
          $attemptNumber: attemptNumber,
          $ownerId: input.ownerId,
          $startedAt: startedAt,
        },
      );
      return {
        attemptId: input.attemptId,
        taskId: input.taskId,
        attemptNumber,
        status: "running",
        ownerId: input.ownerId,
        startedAt,
      };
    }, owner);
  }

  async endAttempt(input: EndAttemptInput): Promise<AttemptRecord> {
    return this.conn.whenFree(() => {
      const row = this.conn.get(
        `SELECT * FROM task_attempts WHERE attempt_id = $id`,
        { $id: input.attemptId },
      ) as AttemptRow | null;
      if (!row) {
        throw new RecordNotFoundError(`Attempt not found: ${input.attemptId}`);
      }
      // The attempt names its task, so the ownership proof is read from the
      // same row the write is about — inside this transaction, next to it.
      // BEFORE the terminal check below: "may you write here?" is a different
      // question from "is there anything to write?".
      if (input.leaseToken !== undefined) {
        this.assertLeaseCurrent(row.task_id, input.leaseToken);
      }
      // FIRST TERMINAL WINS — see `TaskStore.endAttempt`. An attempt already
      // ended is returned as it stands, so a recovery pass acting on an expired
      // lease cannot restate a `completed` attempt as `abandoned` and have the
      // count below read a clean finish as a crash. Reachable in one process:
      // a runner that ends the attempt and then fails to land the task leaves
      // exactly that pair behind for recovery to find.
      if (row.status !== "running") return attemptFromRow(row);
      const endedAt = this.clock.nowIso();
      this.conn.run(
        `UPDATE task_attempts SET status = $status, ended_at = $endedAt, error = COALESCE($error, error) WHERE attempt_id = $id`,
        {
          $status: input.status,
          $endedAt: endedAt,
          $error: input.error ?? null,
          $id: input.attemptId,
        },
      );
      // An abandoned attempt IS the poison event, so the count is written here,
      // in the same transaction as the attempt row — never by the caller on a
      // later transition, where a crash in between loses the death and two
      // callers reading-then-writing lose one of two. Only `abandoned`: a
      // failure that ended cleanly is a different diagnosis (see
      // `TaskRecord.poisonCount`). Only the `running` → `abandoned` EDGE, which
      // the terminal check above already guarantees — a recoverer that ends the
      // same attempt `abandoned` twice reports one death, not two.
      if (input.status === "abandoned") {
        this.conn.run(
          `UPDATE tasks SET poison_count = poison_count + 1 WHERE task_id = $taskId`,
          { $taskId: row.task_id },
        );
      }
      return attemptFromRow({
        ...row,
        status: input.status,
        ended_at: endedAt,
        error: input.error ?? row.error,
      });
    }, this.txOwner);
  }

  async acquireLease(input: AcquireLeaseInput): Promise<Lease> {
    return this.acquireLeaseAs(this.txOwner, input);
  }

  /** {@link acquireLease}, told which transaction it belongs to. */
  private async acquireLeaseAs(
    owner: TxOwner | undefined,
    input: AcquireLeaseInput,
  ): Promise<Lease> {
    return this.conn.whenFree(() => {
      // Store-global monotonic fencing token, single-row counter table.
      this.conn.run(
        `UPDATE fencing_counter SET value = value + 1 WHERE id = 1`,
      );
      const counter = this.conn.get(
        `SELECT value FROM fencing_counter WHERE id = 1`,
      ) as { value: number };
      const fencingToken = counter.value;
      const leaseToken = `lease_${crypto.randomUUID()}`;
      const expiresAt = new Date(
        this.clock.now().getTime() + input.ttlMs,
      ).toISOString();
      // PK on task_id: this always mints a fresh lease, replacing whatever was
      // there — see the module doc on lease semantics.
      this.conn.run(
        `INSERT INTO leases (task_id, attempt_id, owner_id, lease_token, fencing_token, expires_at)
         VALUES ($taskId, $attemptId, $ownerId, $leaseToken, $fencingToken, $expiresAt)
         ON CONFLICT(task_id) DO UPDATE SET
           attempt_id = excluded.attempt_id,
           owner_id = excluded.owner_id,
           lease_token = excluded.lease_token,
           fencing_token = excluded.fencing_token,
           expires_at = excluded.expires_at`,
        {
          $taskId: input.taskId,
          $attemptId: input.attemptId,
          $ownerId: input.ownerId,
          $leaseToken: leaseToken,
          $fencingToken: fencingToken,
          $expiresAt: expiresAt,
        },
      );
      return {
        taskId: input.taskId,
        attemptId: input.attemptId,
        ownerId: input.ownerId,
        leaseToken,
        fencingToken,
        expiresAt,
      };
    }, owner);
  }

  /**
   * Extend a lease that is still alive.
   *
   * AN EXPIRED LEASE IS NOT RENEWABLE, even while its row survives — the row
   * only outlives the expiry until someone runs `expireStaleLeases`, and the
   * whole point of an expiry is that another owner may act on it from that
   * instant. Renewing across it would resurrect ownership recovery is entitled
   * to consider gone, and it would break `renewLease`'s second job: the runner
   * uses it as the fencing probe ("may I still write?"), and a probe that says
   * yes on an expired lease is the wrong answer to that question.
   */
  async renewLease(leaseToken: string, ttlMs: number): Promise<Lease> {
    return this.conn.whenFree(() => {
      const now = this.clock.now();
      const row = this.conn.get(
        `SELECT * FROM leases WHERE lease_token = $token`,
        {
          $token: leaseToken,
        },
      ) as LeaseRow | null;
      if (!row) {
        throw new LeaseLostError(`Lease token ${leaseToken} is not current.`, {
          leaseToken,
        });
      }
      // `<=` matches `expireStaleLeases`, so the two never disagree about a
      // lease that expires exactly on the instant being asked about.
      if (new Date(row.expires_at).getTime() <= now.getTime()) {
        throw new LeaseLostError(
          `Lease token ${leaseToken} expired at ${row.expires_at}.`,
          { leaseToken, expiresAt: row.expires_at },
        );
      }
      const expiresAt = new Date(now.getTime() + ttlMs).toISOString();
      this.conn.run(
        `UPDATE leases SET expires_at = $expiresAt WHERE lease_token = $token`,
        { $expiresAt: expiresAt, $token: leaseToken },
      );
      return leaseFromRow({ ...row, expires_at: expiresAt });
    }, this.txOwner);
  }

  async releaseLease(leaseToken: string): Promise<void> {
    await this.conn.whenFree(() => {
      const result = this.conn.run(
        `DELETE FROM leases WHERE lease_token = $token`,
        {
          $token: leaseToken,
        },
      );
      if (result.changes === 0) {
        throw new LeaseLostError(`Lease token ${leaseToken} is not current.`, {
          leaseToken,
        });
      }
    }, this.txOwner);
  }

  async expireStaleLeases(now: Date): Promise<Lease[]> {
    return this.conn.whenFree(() => {
      const nowIso = now.toISOString();
      const rows = this.conn.all(
        `SELECT * FROM leases WHERE expires_at <= $now`,
        {
          $now: nowIso,
        },
      ) as LeaseRow[];
      if (rows.length > 0) {
        this.conn.run(`DELETE FROM leases WHERE expires_at <= $now`, {
          $now: nowIso,
        });
      }
      return rows.map(leaseFromRow);
    }, this.txOwner);
  }

  async appendEvents(
    taskId: string,
    events: TaskEventEnvelope[],
    opts: AppendEventsOptions,
  ): Promise<void> {
    if (events.length === 0) return;
    await this.conn.whenFree(() => {
      const lease = this.conn.get(
        `SELECT lease_token FROM leases WHERE task_id = $taskId`,
        { $taskId: taskId },
      ) as { lease_token: string } | null;
      if (!lease || lease.lease_token !== opts.leaseToken) {
        throw new LeaseLostError(
          `Lease token ${opts.leaseToken} is not current for task ${taskId}.`,
          { taskId, leaseToken: opts.leaseToken },
        );
      }
      const lastRow = this.conn.get(
        `SELECT MAX(seq) as maxSeq FROM task_events WHERE task_id = $taskId`,
        { $taskId: taskId },
      ) as { maxSeq: number | null };
      let last = lastRow.maxSeq ?? -1;
      // Validate the whole batch before writing anything.
      for (const event of events) {
        if (event.seq <= last) {
          throw new SeqConflictError(
            `Non-monotonic seq ${event.seq} for task ${taskId} (last ${last}).`,
            { taskId, seq: event.seq, last },
          );
        }
        last = event.seq;
      }
      for (const event of events) {
        try {
          this.conn.run(
            `INSERT INTO task_events (task_id, seq, event_id, attempt_id, type, timestamp, payload)
             VALUES ($taskId, $seq, $eventId, $attemptId, $type, $timestamp, $payload)`,
            {
              $taskId: taskId,
              $seq: event.seq,
              $eventId: event.eventId,
              $attemptId: event.attemptId ?? null,
              $type: event.type,
              $timestamp: event.timestamp,
              $payload: toJson(event),
            },
          );
        } catch (err) {
          if (isConstraintError(err)) {
            throw new SeqConflictError(
              `Duplicate seq or eventId for task ${taskId} (seq ${event.seq}).`,
              { taskId, seq: event.seq, cause: String(err) },
            );
          }
          throw err;
        }
      }
    }, this.txOwner);
  }

  async listEvents(
    taskId: string,
    opts?: ListEventsOptions,
  ): Promise<TaskEventEnvelope[]> {
    let sql = `SELECT * FROM task_events WHERE task_id = $taskId`;
    const params: Params = { $taskId: taskId };
    if (opts?.afterSeq !== undefined) {
      sql += ` AND seq > $after`;
      params.$after = opts.afterSeq;
    }
    sql += ` ORDER BY seq ASC`;
    if (opts?.limit !== undefined) {
      sql += ` LIMIT $limit`;
      params.$limit = opts.limit;
    }
    const rows = this.conn.all(sql, params) as TaskEventRow[];
    return rows.map((row) => parseJson<TaskEventEnvelope>(row.payload));
  }

  async nextSeq(taskId: string): Promise<number> {
    const row = this.conn.get(
      `SELECT MAX(seq) as maxSeq FROM task_events WHERE task_id = $taskId`,
      { $taskId: taskId },
    ) as { maxSeq: number | null };
    return (row.maxSeq ?? -1) + 1;
  }

  async updateProgress(
    taskId: string,
    progress: Record<string, unknown>,
    opts: UpdateProgressOptions,
  ): Promise<TaskRecord> {
    return this.conn.whenFree(() => {
      const row = this.selectTaskRow(taskId);
      if (!row) throw new RecordNotFoundError(`Task not found: ${taskId}`);
      // The same ownership proof `appendEvents` demands, read inside the same
      // transaction as the write it guards.
      const lease = this.conn.get(
        `SELECT lease_token FROM leases WHERE task_id = $taskId`,
        { $taskId: taskId },
      ) as { lease_token: string } | null;
      if (!lease || lease.lease_token !== opts.leaseToken) {
        throw new LeaseLostError(
          `Lease token ${opts.leaseToken} is not current for task ${taskId}.`,
          { taskId, leaseToken: opts.leaseToken },
        );
      }
      // Plain assignment, not COALESCE: progress is an overwritten snapshot,
      // and the whole shape belongs to the latest writer.
      this.conn.run(
        `UPDATE tasks SET progress = $progress WHERE task_id = $id`,
        {
          $progress: toJson(progress),
          $id: taskId,
        },
      );
      return taskFromRow(this.selectTaskRow(taskId)!);
    }, this.txOwner);
  }

  /**
   * A claim is one transaction of its own: task row, attempt and lease land
   * together or not at all.
   *
   * OF ITS OWN is the load-bearing part, and it is the connection's FIFO that
   * provides it (see {@link SqliteConnection.withAsyncTx}). `claimNext` awaits
   * inside its candidate walk, so overlapping calls used to flatten into ONE
   * transaction and make the second caller's grant hostage to the first: a
   * rollback anywhere on that shared path reverted the task row to `queued`
   * while the attempt and lease written afterwards committed, and a later
   * `claimNext` handed the same task to a second worker. The same happened to a
   * claim that arrived while an unrelated `AssistantStore.transaction` was
   * open.
   *
   * A claim issued through the `tx` view of an open transaction is the one
   * caller that still joins it — {@link txOwner} is set on that copy, and such
   * a caller asked for one unit of work.
   */
  async claimNext(input: ClaimNextInput): Promise<ClaimedTask | null> {
    // `owner` is the transaction THIS call opened (or, on the flattened path,
    // the caller's). Every write below is made through it, because they belong
    // to the claim's own unit of work — a `whenFree` gated on `this.txOwner`
    // would wait for the transaction it is already running inside.
    return this.conn.withAsyncTx(async (owner) => {
      const nowIso = input.now.toISOString();
      const rows = this.selectClaimCandidates(
        nowIso,
        input.scopesBusy,
        input.kinds,
      );
      // Walk the ordered candidates rather than taking the first: the head of
      // the queue can be gated on a dependency still in flight, or doomed by
      // one that failed, and neither may hide the claimable work behind it.
      //
      // The rows are a SNAPSHOT. The connection's gate keeps two `claimNext`
      // calls apart, but nothing stops another caller settling or claiming
      // one of these tasks between the SELECT and this row's turn — so a lost
      // `queued`-> CAS means someone else got there first, which is the race
      // resolving normally, not a fault: skip the row and keep walking.
      for (const row of rows) {
        const verdict = evaluateTaskDependencies(this.dependencyStates(row));
        if (verdict.kind === "blocked") continue;
        if (verdict.kind === "settle") {
          // Settled here, on the claim path, instead of by a background sweep
          // — see TaskStore.claimNext. The scan then continues past it.
          try {
            await this.transitionTaskAs(
              owner,
              row.task_id,
              ["queued"],
              verdict.to,
              {
                finishedAt: this.clock.nowIso(),
                ...(verdict.error === undefined
                  ? {}
                  : { error: verdict.error }),
              },
            );
          } catch (err) {
            if (!(err instanceof InvalidTaskTransitionError)) throw err;
          }
          continue;
        }
        let task: TaskRecord;
        try {
          task = await this.transitionTaskAs(
            owner,
            row.task_id,
            ["queued"],
            "running",
            { startedAt: this.clock.nowIso() },
          );
        } catch (err) {
          if (!(err instanceof InvalidTaskTransitionError)) throw err;
          continue;
        }
        const attempt = await this.createAttemptAs(owner, {
          attemptId: this.ids.attemptId(),
          taskId: task.taskId,
          ownerId: input.ownerId,
        });
        const lease = await this.acquireLeaseAs(owner, {
          taskId: task.taskId,
          attemptId: attempt.attemptId,
          ownerId: input.ownerId,
          ttlMs: this.leaseTtlMs,
        });
        return { task, attempt, lease };
      }
      return null;
    }, this.txOwner);
  }

  async markDeadLettered(
    taskId: string,
    reason: string,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord> {
    return this.conn.whenFree(() => {
      const row = this.selectTaskRow(taskId);
      if (!row) throw new RecordNotFoundError(`Task not found: ${taskId}`);
      if (opts?.leaseToken !== undefined) {
        this.assertLeaseCurrent(taskId, opts.leaseToken);
      }
      const at = this.clock.nowIso();
      this.conn.run(
        `UPDATE tasks SET dead_lettered_at = $at, dead_letter_reason = $reason WHERE task_id = $id`,
        { $at: at, $reason: reason, $id: taskId },
      );
      return taskFromRow(this.selectTaskRow(taskId)!);
    }, this.txOwner);
  }

  /**
   * Refuse a write whose `leaseToken` is not the task's CURRENT lease.
   *
   * READ INSIDE THE CALLER'S TRANSACTION, which is the entire point: a runner
   * can only check ownership and then write across two awaits, and the gap is
   * where a zombie attempt lands a verdict over the live one's. Here the proof
   * and the write cannot be separated.
   *
   * The lease table is one row per task (PK on `task_id`, replaced by every
   * `acquireLease`), so the row this reads always carries the HIGHEST fencing
   * token ever issued for the task — matching the token therefore IS the
   * fencing comparison, with no second value to compare. The token is reported
   * in `details` so an operator can tell "your generation was superseded" from
   * "there is no lease at all".
   */
  private assertLeaseCurrent(taskId: string, leaseToken: string): void {
    const lease = this.conn.get(
      `SELECT lease_token, fencing_token FROM leases WHERE task_id = $taskId`,
      { $taskId: taskId },
    ) as { lease_token: string; fencing_token: number } | null;
    if (!lease || lease.lease_token !== leaseToken) {
      throw new LeaseLostError(
        `Lease token ${leaseToken} is not current for task ${taskId}.`,
        {
          taskId,
          leaseToken,
          ...(lease === null
            ? {}
            : { currentFencingToken: lease.fencing_token }),
        },
      );
    }
  }

  private selectTaskRow(taskId: string): TaskRow | null {
    return (
      (this.conn.get(`SELECT * FROM tasks WHERE task_id = $id`, {
        $id: taskId,
      }) as TaskRow | undefined) ?? null
    );
  }

  /**
   * The claim query: status/availableAt/scope/kind filters, ordered by
   * effective priority desc, then `enqueued_at` asc, then `rowid` asc as a
   * final deterministic tie-break (insertion order, for when two rows share a
   * millisecond timestamp).
   *
   * Effective priority is `priority + min(maxBonus, floor(waitMs / intervalMs)
   * * bonus)` — the formula in `@agentkit/host`'s `ports/task-aging.ts`,
   * expressed in SQL so the
   * ORDER BY sees it rather than the caller re-sorting a page of rows that was
   * already chosen by the wrong key. With the default `bonus = 0` the term
   * folds to zero and the ordering is plain `priority DESC, enqueued_at ASC`.
   * The wait is computed via `julianday` (days as a float) rather than
   * `strftime('%s')` (whole seconds), because an aging interval shorter than a
   * second is otherwise silently rounded to "no wait at all".
   *
   * RETURNS EVERY CANDIDATE, ordered, not just the first — `claimNext` has to
   * be able to walk past a task its dependencies are still gating. That is a
   * full read of the claimable set for this worker, which is the honest cost of
   * doing dependency gating outside SQL in a reference adapter; a store that
   * expected a very deep queue would push the gate into the query (a
   * `depends_on` edge table with a NOT EXISTS correlated subquery) instead.
   *
   * `$now` is the CALLER-SUPPLIED `ClaimNextInput.now`, bound once and reused
   * in both the WHERE filter and the aging expression — not SQL's own
   * `strftime('%s','now')`. Reading wall-clock independently there would
   * disagree with the `available_at <= $now` filter by however long the
   * query takes to reach that clause, and would make the aging term
   * untestable (a caller cannot advance the database engine's clock, but
   * freely controls what `now` it passes in).
   *
   * An EMPTY `kinds` array means "no kind is acceptable", not "any kind": a
   * worker with an empty executor registry can claim nothing, and quietly
   * treating that as unfiltered would hand it work it cannot run.
   */
  private selectClaimCandidates(
    nowIso: string,
    scopesBusy: string[],
    kinds: string[] | undefined,
  ): TaskRow[] {
    let sql = `SELECT * FROM tasks WHERE status = 'queued' AND available_at <= $now`;
    const params: Params = {
      $now: nowIso,
      $agingIntervalMs: this.aging.intervalMs,
      $agingBonus: this.aging.bonus,
      $agingMaxBonus: this.aging.maxBonus,
    };
    if (scopesBusy.length > 0) {
      const placeholders = scopesBusy.map((_, i) => `$busy${i}`).join(", ");
      sql += ` AND scope_id NOT IN (${placeholders})`;
      scopesBusy.forEach((scope, i) => {
        params[`$busy${i}`] = scope;
      });
    }
    if (kinds !== undefined) {
      if (kinds.length === 0) return [];
      const placeholders = kinds.map((_, i) => `$kind${i}`).join(", ");
      sql += ` AND kind IN (${placeholders})`;
      kinds.forEach((kind, i) => {
        params[`$kind${i}`] = kind;
      });
    }
    sql += ` ORDER BY (priority + MIN($agingMaxBonus,
               CAST(MAX(0, (julianday($now) - julianday(enqueued_at)) * 86400000.0)
                    / $agingIntervalMs AS INTEGER) * $agingBonus)) DESC,
             enqueued_at ASC, rowid ASC`;
    return this.conn.all(sql, params) as TaskRow[];
  }

  /** The narrow projection {@link evaluateTaskDependencies} grades. */
  private dependencyStates(row: TaskRow): TaskDependencyState[] {
    if (row.depends_on === null) return [];
    return parseJson<string[]>(row.depends_on).map((dependencyId) => {
      const dependency = this.conn.get(
        `SELECT status, dead_lettered_at FROM tasks WHERE task_id = $id`,
        { $id: dependencyId },
      ) as { status: string; dead_lettered_at: string | null } | null;
      return {
        taskId: dependencyId,
        status: dependency === null ? null : (dependency.status as TaskStatus),
        deadLettered:
          dependency !== null && dependency.dead_lettered_at !== null,
      };
    });
  }

  /**
   * Prove every edge a new task declares points at a row that already exists —
   * the acyclicity guarantee, enforced at write time. See
   * {@link UnknownDependencyError}.
   */
  private assertDependenciesExist(
    taskId: string,
    parentTaskId: string | undefined,
    dependsOn: string[] | null,
  ): void {
    if (
      parentTaskId !== undefined &&
      this.selectTaskRow(parentTaskId) === null
    ) {
      throw new UnknownDependencyError(
        `Task ${taskId} names parent ${parentTaskId}, which does not exist.`,
        { taskId, parentTaskId },
      );
    }
    for (const dependency of dependsOn ?? []) {
      // Self-dependency first and by identity: the row is not written yet, so
      // a plain existence check would report the wrong reason for it.
      if (dependency === taskId || this.selectTaskRow(dependency) === null) {
        throw new UnknownDependencyError(
          `Task ${taskId} depends on ${dependency}, which does not exist.`,
          { taskId, dependsOn: dependency },
        );
      }
    }
  }
}
