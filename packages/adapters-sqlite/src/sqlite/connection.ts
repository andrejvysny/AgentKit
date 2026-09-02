/**
 * The connection wrapper every sub-store shares: flattened
 * `BEGIN IMMEDIATE` / `COMMIT` / `ROLLBACK`, the FIFO write gate that keeps
 * concurrent transactions on one handle from stepping on each other, and the
 * busy-lock retry loop for several handles over one file.
 *
 * Split out of `sqlite-assistant-store.ts` — every sub-store module and the
 * aggregate itself depend on {@link SqliteConnection} and {@link TxOwner}, so
 * this is the one file with no dependency back on the rest of the package.
 */
import type { Changes, Database } from "bun:sqlite";
import { TransactionGateTimeoutError } from "@agentkit/host";

/**
 * How long a caller waits for another caller's open transaction before giving
 * up with {@link TransactionGateTimeoutError}.
 *
 * Generous enough that no honest transaction can trip it — a callback holding
 * the gate for half a minute has a problem the store cannot fix — and finite,
 * which is the whole point: the wait it bounds is the one a caller can create
 * for itself, and an unbounded version of it is indistinguishable from a hang.
 */
export const DEFAULT_TRANSACTION_GATE_TIMEOUT_MS = 30_000;
/**
 * How long a transaction waits for another connection's write lock.
 *
 * Generous on purpose: the cost of waiting is latency, and the cost of not
 * waiting is a raw `SQLITE_BUSY` surfacing out of a port method that documents
 * no such failure. See the multi-handle section on {@link SqliteConnection}.
 */
export const DEFAULT_BUSY_TIMEOUT_MS = 5_000;

/**
 * Whether a driver error means "someone else holds the lock, try again" —
 * SQLITE_BUSY and its variants, plus SQLITE_LOCKED.
 *
 * A prefix match rather than an equality one: SQLite reports extended codes
 * (`SQLITE_BUSY_SNAPSHOT`, `SQLITE_BUSY_TIMEOUT`) whose meaning for a caller is
 * identical — the write lock was not available — and enumerating them would
 * only mean missing whichever one a future SQLite adds.
 */
function isBusyError(err: unknown): boolean {
  const code = (err as { code?: string } | null)?.code;
  return (
    typeof code === "string" &&
    (code.startsWith("SQLITE_BUSY") || code.startsWith("SQLITE_LOCKED"))
  );
}

// ---------------------------------------------------------------------------
// Connection wrapper: flattened BEGIN IMMEDIATE / COMMIT / ROLLBACK
// ---------------------------------------------------------------------------

/** Bound-parameter bag: `$name` keys, scalar/null values — the named-params style used throughout this file. */
export type Params = Record<string, string | number | boolean | bigint | null>;

/**
 * Identity of one open async transaction — a token, not a counter.
 *
 * "A transaction is open" and "MY transaction is open" are different questions,
 * and only the second one may flatten; see
 * {@link SqliteConnection.withAsyncTx}. Deliberately opaque: nothing reads a
 * field on it, callers only ever compare it by identity.
 */
export interface TxOwner {
  readonly open: true;
}

/**
 * One caller's BOUNDED wait on {@link SqliteConnection.txGate}.
 *
 * WHY A WATCHDOG. The gate's holder always settles — unless the caller waiting
 * on it is the reason the holder cannot finish. A `transaction()` callback that
 * awaits a ROOT-store call is exactly that shape: the call queues behind the
 * transaction it is running inside, which cannot commit until the callback
 * returns. That used to park the request forever, with no error anywhere and
 * nothing in a log to read; the bound turns it into a
 * {@link TransactionGateTimeoutError} whose stack points at the callback.
 *
 * WHY THE QUEUE ENTRY OUTLIVES THE CALLER. A timed-out caller is rejected, but
 * its place in the FIFO stays and cancels itself when its turn comes
 * ({@link arrive}). Dropping the entry instead would settle the promise the
 * NEXT caller is already chained to while the transaction it was queued behind
 * is still open — and that caller would then run its `BEGIN` inside a `BEGIN`.
 */
class GateWait {
  private expired = false;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private readonly expiry: Promise<never>;

  constructor(private readonly timeoutMs: number) {
    this.expiry = new Promise<never>((_resolve, reject) => {
      // Non-finite or non-positive is the documented opt-out: no timer, and the
      // wait is unbounded exactly as it was before this class existed.
      if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) return;
      const timer = setTimeout(() => {
        this.expired = true;
        reject(new TransactionGateTimeoutError(timeoutMs));
      }, timeoutMs);
      // A watchdog must never be the reason a process stays alive.
      (timer as unknown as { unref?: () => void }).unref?.();
      this.timer = timer;
    });
  }

  /** Whichever comes first: the caller's turn, or the deadline. */
  race<T>(work: Promise<T>): Promise<T> {
    return Promise.race([work, this.expiry]);
  }

  /** This caller's turn arrived: stop the clock, or refuse if it already ran out. */
  arrive(): void {
    this.cancel();
    if (this.expired) throw new TransactionGateTimeoutError(this.timeoutMs);
  }

  /** Stop the clock — the wait is over, however it ended. */
  cancel(): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
  }
}

/**
 * Every {@link SqliteConnection} ever built, keyed by the handle it wraps.
 *
 * A WeakMap so a closed store's connection is collectable with its `Database`
 * — this registry must not be the reason either stays alive. See
 * {@link writeGateFor} for what reads it.
 */
export const connectionsByHandle = new WeakMap<Database, SqliteConnection>();

/**
 * Shared by every sub-store so a multi-statement operation (inside one port
 * method, or spanning several via {@link SqliteAssistantStore.transaction})
 * commits or rolls back as one unit. Also the single seam where bun-types'
 * generic (array-rest) binding signature is cast to the named-params object
 * form its own runtime and JSDoc document (`db.run(sql, { $name: "foo" })`)
 * — the shipped `.d.ts` models positional array bindings precisely but not
 * that form, so callers here pass a plain `{ $x: ... }` object and this class
 * is the only place that casts it.
 *
 * `bun:sqlite` is synchronous and does not support nested transactions on one
 * connection (no savepoints in this v1 — see the class doc on
 * {@link SqliteAssistantStore.transaction}), so re-entrant calls FLATTEN into
 * the transaction already open: only the outermost `withTx`/`withAsyncTx`
 * issues BEGIN/COMMIT/ROLLBACK.
 *
 * WHICH CALLS COUNT AS RE-ENTRANT IS DECIDED BY OWNERSHIP, NOT BY DEPTH. A
 * raised `txDepth` says "a transaction is open", never "mine is open", and an
 * unrelated caller that flattened on it made its whole unit of work hostage to
 * a stranger's rollback: a second `AssistantStore.transaction` caller reported
 * a commit its neighbour's throw then erased, and a `claimNext` that landed in
 * a host transaction had its claim reverted under a worker already holding the
 * lease. So the two helpers answer the question differently:
 *
 * - {@link withTx} (synchronous) still flattens on depth. It holds the thread
 *   from BEGIN to COMMIT, so nothing can interleave WITH it, and flattening is
 *   what lets another object over this same handle — `SqliteMcpServerConfigStore`
 *   — write inside an open transaction instead of deadlocking against it.
 * - {@link withAsyncTx} flattens only for the caller holding the CURRENT owner
 *   token. Every other caller queues behind {@link txGate} and gets its own
 *   BEGIN, so one caller's rollback can only ever discard that caller's work.
 *
 * That left one hole, which {@link whenFree} closes: an unrelated caller's
 * SYNCHRONOUS port write, issued while an async transaction sat on an `await`,
 * still joined that transaction on plain `withTx` — and was erased by a
 * rollback it had nothing to do with. Every WRITE method of every sub-store now
 * goes through `whenFree`, which waits out a transaction it does not own before
 * opening its own. READS still join: they take no locks worth serializing, and
 * a read that queued behind a transaction it is not part of would turn every
 * `getTask` inside a busy host into a wait.
 *
 * ── SEVERAL HANDLES OVER ONE FILE ─────────────────────────────────────────
 *
 * Supported, and this class is where the support lives. Two
 * {@link SqliteAssistantStore} instances on one path — two worker processes, or
 * two connections in one process — are two connections contending for SQLite's
 * single write lock, and this connection's own {@link txGate} means nothing
 * across that boundary. `BEGIN IMMEDIATE` is what keeps them correct;
 * what keeps them USABLE is waiting for the lock instead of failing on it, and
 * the two waits are deliberately different:
 *
 * - SYNCHRONOUS transactions ({@link withTx}) wait inside SQLite, via the
 *   `PRAGMA busy_timeout` the store sets on open. They cannot await, and when
 *   the lock holder is another OS process, parking this thread is exactly the
 *   right thing to do.
 * - ASYNCHRONOUS transactions ({@link withAsyncTx} — `claimNext` and
 *   `AssistantStore.transaction`) wait on the EVENT LOOP instead, and set
 *   `busy_timeout` to zero while they try. They hold the lock across `await`s,
 *   so the holder may well be this same process's other handle — and then the
 *   thread SQLite would park is the only thread that could ever release the
 *   lock. Sleeping on it turns a moment of contention into a deadlock that
 *   lasts the whole timeout and then fails anyway.
 */
export class SqliteConnection {
  private txDepth = 0;

  /**
   * The FIFO every transaction and every queued root write takes a slot in:
   * each call chains onto the previous one's SETTLED signal, so they run one at
   * a time on this connection, in call order.
   */
  private txGate: Promise<void> = Promise.resolve();

  /**
   * Slots taken on {@link txGate} and not yet finished — queued as well as
   * running.
   *
   * {@link currentOwner} cannot answer "is the queue empty?": a transaction
   * that has taken its slot has not opened its BEGIN yet, so the owner is still
   * `null` for a turn of the event loop. A write that read only the owner would
   * run ahead of every transaction issued before it and still waiting.
   */
  private gateDepth = 0;

  /** Token of the async transaction currently open, `null` when there is none. */
  private currentOwner: TxOwner | null = null;

  constructor(
    readonly db: Database,
    /** Ceiling on how long either wait above will keep trying. */
    private readonly busyTimeoutMs: number,
    /** Ceiling on how long a caller waits for THIS connection's gate — see {@link GateWait}. */
    private readonly gateTimeoutMs: number = DEFAULT_TRANSACTION_GATE_TIMEOUT_MS,
  ) {
    // One queue per HANDLE, findable from the handle alone — see
    // {@link writeGateFor}. A second store over this same connection has to
    // queue on THIS gate; a gate of its own would serialize nothing.
    connectionsByHandle.set(db, this);
  }

  run(sql: string, params?: Params): Changes {
    // bun-types' generic for Database.run (`...bindings: ParamsType[]` where
    // `ParamsType extends SQLQueryBindings[]`) models an array of bindings
    // ARRAYS, which does not match its own documented single-object calling
    // convention (`db.run(sql, { $name: "foo" })`, per the class's own
    // JSDoc). Re-typing `this.db` sidesteps that mismatched generic while
    // still calling `run` AS A METHOD on the same instance (not a detached
    // function reference — bun:sqlite's native binding needs `this` bound to
    // the Database instance, so extracting `db.run` into a bare variable and
    // calling it unbound breaks at runtime even though it type-checks).
    const db = this.db as unknown as {
      run(sql: string, params?: Params): Changes;
    };
    return params === undefined ? db.run(sql) : db.run(sql, params);
  }

  // biome-ignore lint/suspicious/noExplicitAny: driver boundary — bun:sqlite rows are untyped; every call site casts to its own Row type immediately
  get(sql: string, params?: Params): any {
    const stmt = this.db.query(sql);
    return params === undefined ? stmt.get() : stmt.get(params);
  }

  // biome-ignore lint/suspicious/noExplicitAny: driver boundary — see get()
  all(sql: string, params?: Params): any[] {
    const stmt = this.db.query(sql);
    return params === undefined ? stmt.all() : stmt.all(params);
  }

  exec(sql: string): void {
    this.db.exec(sql);
  }

  /**
   * Synchronous transaction helper for a single port method's own SQL.
   *
   * THE BEGIN RUNS BEFORE `txDepth` MOVES, and that order is load-bearing: a
   * `BEGIN IMMEDIATE` that throws (another connection holds the write lock)
   * used to leave the counter raised forever, so every later call on this
   * connection took the "already in a transaction" branch and ran with no
   * BEGIN, no COMMIT and no ROLLBACK — atomicity silently gone for the life of
   * the connection. Raising the counter only once the transaction really is
   * open makes the pair exception-safe without changing the flatten-on-reentry
   * semantics: a raised counter still means, exactly, "a transaction is open".
   */
  withTx<T>(fn: () => T): T {
    if (this.txDepth > 0) return fn();
    // Waits inside SQLite for up to `busy_timeout` — see the class doc.
    this.exec("BEGIN IMMEDIATE");
    this.txDepth += 1;
    try {
      const result = fn();
      this.exec("COMMIT");
      return result;
    } catch (err) {
      this.rollback();
      throw err;
    } finally {
      this.txDepth -= 1;
    }
  }

  /**
   * Async transaction helper for {@link AssistantStore.transaction} and
   * `claimNext`: `fn` may `await` between its statements, so this transaction
   * is held across turns of the event loop, where anybody else's callback can
   * run.
   *
   * ONE AT A TIME PER CONNECTION, IN CALL ORDER. A caller that arrives while a
   * transaction is open waits on {@link txGate} for it to settle instead of
   * joining it — joining is what let one caller's rollback discard another
   * caller's finished work (see the class doc). The one exception is the caller
   * that IS the open transaction: `owner` names it, and a call carrying the
   * token of the transaction currently running flattens into it, since there
   * are no savepoints to nest with. That is how a nested `transaction()` and a
   * `claimNext` issued through the `tx` view stay inside the unit their caller
   * opened, while the same calls made by anyone else queue.
   *
   * The gate wait is deliberately NOT bounded by `busyTimeoutMs`. That budget
   * exists for the write lock, which another process owns and may never
   * release; the gate is this process's own queue. It IS bounded by
   * `gateTimeoutMs`, which is a different question — not "is the lock free
   * yet?" but "is the holder waiting on me?" — see {@link GateWait}.
   */
  async withAsyncTx<T>(
    fn: (owner: TxOwner) => Promise<T>,
    owner?: TxOwner,
  ): Promise<T> {
    // Decided SYNCHRONOUSLY, on the caller's own turn: `currentOwner` is read
    // before the first await, so it still describes the transaction this call
    // was issued from.
    if (owner !== undefined && owner === this.currentOwner) return fn(owner);
    const waited = new GateWait(this.gateTimeoutMs);
    const run = this.txGate.then(() => {
      // Still in line, and still wanted? `arrive` throws for a caller that
      // already timed out — the BEGIN below must not happen for one.
      waited.arrive();
      return this.beginExclusive(fn);
    });
    this.enqueue(run);
    return waited.race(run);
  }

  /**
   * Take the tail of {@link txGate} for `run`, and leave the next caller a
   * SETTLED signal to chain onto.
   *
   * The signal is settled-only: the next caller waits for this one to finish
   * and must not inherit its rejection. {@link gateDepth} is raised
   * SYNCHRONOUSLY, on the turn the slot is taken, which is what makes arrival
   * order — not "who noticed the gate free first" — the run order.
   */
  private enqueue(run: Promise<unknown>): void {
    this.gateDepth += 1;
    const done = (): void => {
      this.gateDepth -= 1;
    };
    this.txGate = run.then(done, done);
  }

  /**
   * Run `fn` in a synchronous transaction of its own, in ARRIVAL ORDER with the
   * async transactions on this connection.
   *
   * A ROOT WRITE TAKES A REAL SLOT IN {@link txGate}, exactly as
   * {@link withAsyncTx} does, and that is what makes the queue fair. Re-reading
   * the gate after each wait instead — "am I free yet?" — is not fairness but a
   * retry loop, and it starves: every `withAsyncTx` issued after this write has
   * already chained its own `.then` onto the promise this write is waiting on,
   * so it opens its BEGIN first and the write finds the connection busy again,
   * for as long as transactions keep arriving. A measured three overlapping
   * `transaction()` loops were enough to hold an `updateChat` off until it hit
   * the gate timeout — and `appendEvents`, `appendMessage` and `transitionTask`
   * are all on this path.
   *
   * THE GATE EXIT AND THE `withTx` ARE ONE TICK, and that is load-bearing. The
   * obvious shape — an `await ready()` helper followed by the caller's own
   * `withTx` — leaves a microtask gap: a transaction already queued on
   * {@link txGate} runs its BEGIN in that gap, and the write then flattens into
   * the stranger's transaction after all. Here `this.withTx(fn)` runs in the
   * same continuation the slot resolves in, so nothing can open a transaction
   * in between.
   *
   * TWO CALLERS SKIP THE QUEUE. The one that IS the open transaction passes its
   * `owner` and flattens immediately — that is what keeps
   * `tx.conversations.updateChat(...)` inside its caller's unit, and what keeps
   * `claimNext`'s own nested writes from waiting on the transaction they are
   * running inside. And a caller arriving at an EMPTY queue runs synchronously:
   * no slot to take, no timer to arm, so the ordinary write path costs exactly
   * what it did before the queue existed. "Empty" is {@link gateDepth}, not
   * {@link currentOwner} — a transaction that has taken its slot has not opened
   * its BEGIN yet, and a write that jumped ahead of it would be the same
   * unfairness in the other direction.
   *
   * A single-statement write is wrapped too. The BEGIN/COMMIT costs a pair of
   * pragma-free statements and buys the one thing the bare `run` did not have:
   * a blast radius of exactly this write.
   */
  async whenFree<T>(fn: () => T, owner?: TxOwner): Promise<T> {
    if (owner !== undefined && owner === this.currentOwner) {
      return this.withTx(fn);
    }
    if (this.gateDepth === 0) return this.withTx(fn);
    // ONE watchdog for the whole wait: the budget is "how long this write
    // waits", not "how long one queue entry takes".
    const waited = new GateWait(this.gateTimeoutMs);
    const run = this.txGate.then(() => {
      // Still in line, and still wanted? `arrive` throws for a caller that
      // already timed out — the BEGIN below must not happen for one, and the
      // slot is released either way (see enqueue) so the caller behind it is
      // not orphaned.
      waited.arrive();
      return this.withTx(fn);
    });
    this.enqueue(run);
    return waited.race(run);
  }

  /**
   * One async transaction, with {@link txGate} already held by this call.
   *
   * Same BEGIN-then-increment ordering as {@link withTx}, for the same reason,
   * and the same exception-safety: a lock this call never won leaves the
   * counter untouched, and the owner token is minted only once the transaction
   * really is open.
   */
  private async beginExclusive<T>(
    fn: (owner: TxOwner) => Promise<T>,
  ): Promise<T> {
    const deadline = Date.now() + this.busyTimeoutMs;
    // Whoever holds the lock here is on ANOTHER HANDLE: the gate keeps this
    // handle's async transactions apart, and a synchronous one cannot still be
    // open across the await below. So this is the cross-connection wait the
    // class doc describes — yield and retry rather than park the thread, see
    // tryBeginImmediate.
    for (let attempt = 0; ; attempt += 1) {
      const busy = this.tryBeginImmediate();
      if (busy === null) break;
      if (Date.now() >= deadline) throw busy;
      // A macrotask, not a microtask: the lock holder's next step may be queued
      // behind one, and a microtask-only yield would spin without ever letting
      // it run. The short backoff keeps a long wait from burning the loop.
      await new Promise((resolve) =>
        setTimeout(resolve, Math.min(attempt, 10)),
      );
    }
    const owner: TxOwner = { open: true };
    this.currentOwner = owner;
    try {
      const result = await fn(owner);
      this.exec("COMMIT");
      return result;
    } catch (err) {
      this.rollback();
      throw err;
    } finally {
      // Cleared, not restored: the gate guarantees there was no async
      // transaction underneath this one.
      this.currentOwner = null;
      this.txDepth -= 1;
    }
  }

  /**
   * One non-blocking attempt at the write lock: `null` when the transaction is
   * open and `txDepth` has been raised, the SQLITE_BUSY error when it is not.
   *
   * `busy_timeout` is dropped to zero for the attempt and restored after,
   * because SQLite's own wait PARKS THE CALLING THREAD — and when the holder is
   * this process's other handle, that thread is the only one that could ever
   * run the holder's continuation and commit. Measured against a real
   * two-handle claim, the parking version stalls for the whole timeout and then
   * raises SQLITE_BUSY anyway; yielding between attempts resolves the same
   * contention in single-digit milliseconds. Synchronous callers cannot do
   * this, which is why {@link withTx} keeps SQLite's wait — for the
   * cross-PROCESS holder it is aimed at, parking the thread is right.
   */
  private tryBeginImmediate(): unknown | null {
    try {
      this.exec("PRAGMA busy_timeout = 0");
      this.exec("BEGIN IMMEDIATE");
    } catch (err) {
      if (isBusyError(err)) return err;
      throw err;
    } finally {
      this.exec(`PRAGMA busy_timeout = ${this.busyTimeoutMs}`);
    }
    // Raised here, with no await since the BEGIN — see withAsyncTx's comment.
    this.txDepth += 1;
    return null;
  }

  private rollback(): void {
    try {
      this.exec("ROLLBACK");
    } catch {
      // No transaction to roll back — the connection died, or something below
      // COMMIT already ended it. Either way there is nothing left to undo.
    }
  }
}
