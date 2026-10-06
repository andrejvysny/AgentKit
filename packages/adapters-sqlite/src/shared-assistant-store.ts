/**
 * Driver-neutral, complete {@link AssistantStore} — the durable reference
 * adapter. `bun:sqlite` is fully synchronous; every port method here is
 * `async` only because the port interfaces are, so a host can swap this for a
 * network-backed store without changing call sites.
 *
 * Fencing/lease enforcement pattern: a write guarded by a `leaseToken` first
 * reads the task's current lease inside the same transaction, then performs
 * its INSERT/UPDATE, and rejects on mismatch with {@link LeaseLostError} —
 * see {@link SqliteTaskStore.appendEvents}. Compare-and-set operations
 * (`transitionTask`, `ProposalStore.transition`) use a guarded
 * `UPDATE ... WHERE id = ? AND status = ?` and check the driver's reported
 * `changes` count as the backstop against a race the initial SELECT could not
 * see — see {@link SqliteConnection}.
 *
 * This is the composition root: it opens the database, wires the connection
 * gate, builds every sub-store (each in its own module under `sqlite/`), and
 * exposes the aggregate `AssistantStore`. Row shapes and mappers live in
 * `sqlite/rows.js`; the transaction/gate plumbing lives in
 * `sqlite/connection.js`.
 */
import type { SqliteDatabase as Database } from "./sqlite/driver.js";
import {
  defaultClock,
  defaultIds,
  type AssistantStore,
  type Clock,
  type ConversationStore,
  type IdGenerator,
  type OutboxStore,
  type ProposalStore,
  type ProviderStore,
  type ProviderContinuationStore,
  type SettingsStore,
  type TaskAgingOptions,
  type TaskStore,
} from "@agentkit/host";
import {
  connectionsByHandle,
  DEFAULT_BUSY_TIMEOUT_MS,
  DEFAULT_TRANSACTION_GATE_TIMEOUT_MS,
  SqliteConnection,
  type TxOwner,
} from "./sqlite/connection.js";
import { SqliteConversationStore } from "./sqlite/conversation-store.js";
import { SqliteOutboxStore } from "./sqlite/outbox-store.js";
import { SqliteProposalStore } from "./sqlite/proposal-store.js";
import { SqliteProviderStore } from "./sqlite/provider-store.js";
import { SqliteProviderContinuationStore } from "./sqlite/provider-continuation-store.js";
import { SqliteSettingsStore } from "./sqlite/settings-store.js";
import { SqliteTaskStore } from "./sqlite/task-store.js";

/**
 * The write queue of one handle: the seam a store over a SHARED connection
 * takes so its writes wait out a transaction they are not part of, instead of
 * joining it.
 *
 * Deliberately one method wide. A second store over the aggregate's handle
 * needs exactly one thing from `SqliteConnection` — a turn — and everything
 * else about that class (owner tokens, the async transaction path) belongs to
 * the aggregate that owns the connection.
 */
export interface SqliteWriteGate {
  /**
   * Run `fn` in a transaction of its own, after every caller already queued.
   *
   * `fn` must be SYNCHRONOUS: it runs between a `BEGIN IMMEDIATE` and its
   * `COMMIT`, and an `await` in there would hold the write lock across a turn
   * of the event loop that the queue is not holding for it.
   */
  whenFree<T>(fn: () => T): Promise<T>;
}

/**
 * The write gate for `db` — the one the {@link SqliteAssistantStore} over this
 * handle already uses, or a fresh one when nothing else has claimed the handle.
 *
 * WHY A LOOKUP AND NOT A CONSTRUCTOR ARGUMENT: {@link SqliteMcpServerConfigStore}
 * is handed a bare `Database` (that is the documented way to share one
 * connection, and it is what {@link SqliteAssistantStore.database} returns), so
 * the handle is all it has to go on. Reading the driver's own
 * `Database.inTransaction` instead — "someone has a transaction open, join it"
 * — is the `txDepth`-for-ownership mistake the aggregate store already paid
 * for: a config write that joined a stranger's `transaction()` reported success
 * and was then erased by that stranger's rollback.
 *
 * `options` is only consulted when this call MINTS the gate; a handle the
 * aggregate already owns keeps the budgets that store was configured with,
 * because a queue with two different timeouts depending on who is asking is not
 * one queue.
 */
export function writeGateFor(
  db: Database,
  options: { busyTimeoutMs?: number; transactionGateTimeoutMs?: number } = {},
): SqliteWriteGate {
  const existing = connectionsByHandle.get(db);
  if (existing !== undefined) return existing;
  return new SqliteConnection(
    db,
    options.busyTimeoutMs ?? DEFAULT_BUSY_TIMEOUT_MS,
    options.transactionGateTimeoutMs ?? DEFAULT_TRANSACTION_GATE_TIMEOUT_MS,
  );
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/** {@link AssistantStore} minus `transaction` — the six sub-stores alone. */
type AssistantStoreStores = Omit<
  AssistantStore,
  "transaction" | "continuations"
> & {
  continuations: ProviderContinuationStore;
};

export interface SqliteAssistantStoreOptions extends TaskAgingOptions {
  /** Defaults to {@link defaultClock} (real wall-clock). */
  clock?: Clock;
  /** Defaults to {@link defaultIds} (UUID-backed). */
  ids?: IdGenerator;
  /** Lease TTL `claimNext` grants the attempt it creates. Default 30s. */
  leaseTtlMs?: number;
  /** Outbox claim-visibility window. Default 30s. */
  outboxClaimVisibilityMs?: number;
  /**
   * How many delivery attempts one outbox record gets before `claimBatch`
   * stops offering it. Default 10 — see {@link OutboxStore.claimBatch}.
   */
  outboxMaxAttempts?: number;
  /**
   * How long a transaction waits for the write lock before giving up, when
   * another connection on the same file holds it. Default 5s.
   *
   * Only meaningful for a file-backed store opened by more than one handle —
   * see the multi-handle section on {@link SqliteConnection}.
   */
  busyTimeoutMs?: number;
  /**
   * How long a caller waits for ANOTHER caller's open `transaction()` before
   * rejecting with `TransactionGateTimeoutError`. Default 30s.
   *
   * Distinct from {@link busyTimeoutMs}, which is about the file's write lock:
   * this budget is about this handle's own queue, and the failure it makes
   * visible is a caller waiting on itself — a `transaction()` callback that
   * awaited a root-store call. Non-finite or non-positive disables the
   * watchdog, restoring the (silently hanging) unbounded wait.
   */
  transactionGateTimeoutMs?: number;
}

/**
 * Driver-neutral, complete {@link AssistantStore}.
 *
 * `transaction(fn)` opens a real `BEGIN IMMEDIATE` and commits or rolls back
 * around `fn` — unlike `MemoryAssistantStore`, a throw inside `fn` discards
 * every write `fn` made. Nested `transaction()` calls on the `tx` it hands the
 * callback (including a port method that itself opens a mini-transaction, like
 * `transitionTask` or `createAttempt`) are FLATTENED into the outermost one
 * rather than nested — `bun:sqlite` has no savepoint support in this v1, so
 * re-entrant calls just run against the already-open transaction.
 *
 * WRITES ARE SERIALIZED PER CONNECTION: a second caller's `transaction()`, a
 * worker's `claimNext`, and every ordinary WRITE method issued while a
 * transaction is open all WAIT for it, and then run in a transaction of their
 * own. They used to join the open one and be rolled back by a stranger's throw.
 * Reads are exempt and still join, because they take no lock worth serializing.
 *
 * THE COROLLARY IS THAT A CALLBACK MUST DO ITS WORK THROUGH THE `tx` IT IS
 * GIVEN. A write made on the ROOT store from inside the callback is, by
 * construction, indistinguishable from an unrelated caller's, so awaiting one
 * in there waits on a transaction that cannot finish until the callback
 * returns. That was already true of a root-store `transaction()`/`claimNext`;
 * it is now true of `store.conversations.updateChat(...)` and every other
 * write. That wait is BOUNDED ({@link SqliteAssistantStoreOptions.transactionGateTimeoutMs},
 * default 30s): the mistake surfaces as a `TransactionGateTimeoutError` naming
 * its cause instead of a request that never returns. See
 * {@link SqliteConnection} and {@link GateWait}.
 */
export class SharedSqliteAssistantStore implements AssistantStore {
  private readonly conn: SqliteConnection;
  /** Every sub-store, bound to one open transaction — see {@link txView}. */
  private readonly viewFor: (owner: TxOwner) => AssistantStoreStores;
  readonly conversations: ConversationStore;
  readonly tasks: TaskStore;
  readonly proposals: ProposalStore;
  readonly providers: ProviderStore;
  readonly settings: SettingsStore;
  readonly outbox: OutboxStore;
  readonly continuations: ProviderContinuationStore;

  constructor(db: Database, options: SqliteAssistantStoreOptions = {}) {
    const busyTimeoutMs = options.busyTimeoutMs ?? DEFAULT_BUSY_TIMEOUT_MS;
    this.conn = new SqliteConnection(
      db,
      busyTimeoutMs,
      options.transactionGateTimeoutMs ?? DEFAULT_TRANSACTION_GATE_TIMEOUT_MS,
    );
    const clock = options.clock ?? defaultClock;
    const ids = options.ids ?? defaultIds;
    // SECOND, IDENTICALLY-CONFIGURED INSTANCES rather than a mutable field on
    // the root ones: the token belongs to ONE transaction, and a field would
    // leak it to every other caller of `store.tasks` for as long as that
    // transaction is open — the exact confusion the token exists to end.
    const build = (owner?: TxOwner): AssistantStoreStores => ({
      conversations: new SqliteConversationStore(this.conn, clock, ids, owner),
      tasks: new SqliteTaskStore(
        this.conn,
        clock,
        ids,
        options.leaseTtlMs,
        options,
        owner,
      ),
      proposals: new SqliteProposalStore(this.conn, clock, owner),
      providers: new SqliteProviderStore(this.conn, owner),
      continuations: new SqliteProviderContinuationStore(
        this.conn,
        clock,
        owner,
      ),
      settings: new SqliteSettingsStore(this.conn, owner),
      outbox: new SqliteOutboxStore(
        this.conn,
        clock,
        options.outboxClaimVisibilityMs,
        options.outboxMaxAttempts,
        owner,
      ),
    });
    const root = build();
    this.conversations = root.conversations;
    this.tasks = root.tasks;
    this.proposals = root.proposals;
    this.providers = root.providers;
    this.continuations = root.continuations;
    this.settings = root.settings;
    this.outbox = root.outbox;
    this.viewFor = (owner) => build(owner);
  }

  async transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T> {
    return this.conn.withAsyncTx((owner) => fn(this.txView(owner)));
  }

  /**
   * The aggregate as seen from INSIDE the transaction `owner` opened.
   *
   * The port already says `transaction` hands its callback "a store view scoped
   * to that transaction"; this is that view, and it is no longer `this` because
   * `this` carries no transaction identity. EVERY sub-store here is an
   * owner-bearing copy, not the root instance: a write now waits out a
   * transaction it does not own ({@link SqliteConnection.whenFree}), so a root
   * instance used in here would queue behind the very transaction it is running
   * inside and never finish. Carrying the owner is also what makes the
   * distinction meaningful in the other direction — `store.conversations` while
   * this transaction is open is a stranger's write, and waits.
   */
  private txView(owner: TxOwner): AssistantStore {
    const view = this.viewFor(owner);
    return {
      ...view,
      transaction: <T>(nested: (tx: AssistantStore) => Promise<T>) =>
        this.conn.withAsyncTx((nestedOwner) => {
          // `nestedOwner` is `owner` on the flattened path and a fresh token
          // only if this view outlived its transaction and had to open a new
          // one — either way the nested callback gets the view that matches the
          // transaction it is actually running in.
          return nested(this.txView(nestedOwner));
        }, owner),
    };
  }

  /**
   * The open handle, for a store over the SAME database that is not part of
   * this aggregate — {@link SqliteMcpServerConfigStore} is the one this exists
   * for.
   *
   * Sharing the handle rather than opening a second one is the point: one
   * connection means one write lock and one transaction depth, so a config
   * write issued while `transaction()` is open flattens into it instead of
   * deadlocking against it. A caller that takes this must NOT close it — the
   * aggregate owns the connection's lifetime, through {@link close}.
   */
  get database(): Database {
    return this.conn.db;
  }

  /** Closes the underlying connection. Safe to call once; further use throws. */
  close(): void {
    this.conn.db.close();
  }
}
