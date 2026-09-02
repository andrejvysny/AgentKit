import type { ConversationStore } from "./conversation-store.js";
import type { OutboxStore } from "./outbox-store.js";
import type { ProposalStore } from "./proposal-store.js";
import type { ProviderStore } from "./provider-store.js";
import type { SettingsStore } from "./settings-store.js";
import type { TaskStore } from "./task-store.js";

/**
 * The host's persistence, as one aggregate.
 *
 * The six stores are grouped rather than injected separately because the
 * operations that matter span them — submitting a turn writes two messages AND a
 * task; finishing one writes events AND a message AND a status — and those writes
 * must land together or not at all. A single aggregate with one
 * {@link AssistantStore.transaction} is what makes that expressible; six
 * independent stores would leave every consumer to invent its own atomicity.
 */
export interface AssistantStore {
  conversations: ConversationStore;
  tasks: TaskStore;
  proposals: ProposalStore;
  providers: ProviderStore;
  settings: SettingsStore;
  outbox: OutboxStore;

  /**
   * Run `fn` in a transaction, handing it a store view scoped to that
   * transaction. Everything `fn` writes through `tx` commits together; a throw
   * rolls all of it back.
   *
   * Implementations need not support nesting: a host that cannot nest should
   * pass the ambient transaction through rather than opening a second one.
   *
   * CALLERS ARE SERIALIZED PER STORE HANDLE. A second `transaction()`, and a
   * worker's `claimNext`, issued while one is open WAIT for it and then run as
   * a unit of their own, so one caller's rollback can only ever discard that
   * caller's writes. Calls the callback makes through the `tx` it is handed
   * JOIN that transaction, and a nested `tx.transaction(...)` flattens into it
   * rather than nesting.
   *
   * THE COROLLARY IS THAT A CALLBACK MUST WORK THROUGH `tx`. A call issued on
   * the ROOT store from inside a callback is, by construction,
   * indistinguishable from an unrelated caller's: it waits for a transaction
   * that cannot finish until the callback returns. That wait is bounded, so it
   * ends in an `AgentKitHostError` with code `transaction_gate_timeout`
   * (`TransactionGateTimeoutError`) rather than a hang — but the fix is always
   * to use `tx`, never to raise the budget. Keep foreign async work out of the
   * callback entirely: do the awaiting outside and pass the results in.
   *
   * ISOLATION CAVEAT: what `transaction()` promises is atomicity and the
   * serialization above, NOT snapshot isolation. READS from other callers still
   * see the store mid-transaction (they take no lock worth serializing), and an
   * adapter may let an ordinary single-call WRITE from another caller queue
   * behind an open transaction — `SqliteAssistantStore` does exactly that, so
   * such a write is delayed rather than joined and rolled back.
   *
   * Both reference adapters now answer the four questions above the same way:
   * `MemoryAssistantStore` has no rollback (it declares
   * `capabilities.atomicTransactions: false`) and does not queue ordinary
   * writes, but serializes, flattens and times out exactly as
   * `SqliteAssistantStore` does. The shared conformance suite in
   * `@agentkit/testing` pins that for any adapter a host writes later.
   */
  transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T>;
}
