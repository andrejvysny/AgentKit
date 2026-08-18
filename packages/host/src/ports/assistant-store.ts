import type { ConversationStore } from "./conversation-store.js";
import type { OutboxStore } from "./outbox-store.js";
import type { ProposalStore } from "./proposal-store.js";
import type { ProviderStore } from "./provider-store.js";
import type { RunStore } from "./run-store.js";
import type { SettingsStore } from "./settings-store.js";

/**
 * The host's persistence, as one aggregate.
 *
 * The six stores are grouped rather than injected separately because the
 * operations that matter span them — submitting a turn writes two messages AND a
 * run; finishing one writes events AND a message AND a status — and those writes
 * must land together or not at all. A single aggregate with one
 * {@link AssistantStore.transaction} is what makes that expressible; six
 * independent stores would leave every consumer to invent its own atomicity.
 */
export interface AssistantStore {
  conversations: ConversationStore;
  runs: RunStore;
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
   * ISOLATION CAVEAT: `transaction()` provides no isolation from concurrent
   * operations on the same store instance; over `bun:sqlite`, concurrent store
   * calls issued while a transaction callback awaits genuinely-async work JOIN
   * that transaction and roll back with it — keep transaction callbacks free of
   * foreign async work. Do the awaiting outside, and pass the results in.
   */
  transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T>;
}
