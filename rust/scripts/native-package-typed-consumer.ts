/**
 * Strict typed-consumer probe for the PACKED `@agentkit/native`.
 *
 * Compiled by `native-package-consumer.mjs` (mode `types`) inside the
 * isolated consumer directory — outside the repository, against the
 * INSTALLED package, with `skipLibCheck` OFF, `strict` on and NodeNext
 * resolution. This is the check the workspace silently passes and a clean
 * install must not: external review reproduced
 * `dist/assistant-store.d.ts(21): error TS2307: Cannot find module
 * '@agentkit/host'` against the packed tarball, because the shipped
 * declarations imported workspace-internal types while the manifest listed
 * them only as devDependencies.
 *
 * Deliberately uses NO Node/Bun/DOM globals and only public named exports,
 * so it compiles under `"types": []` — the declarations themselves must
 * resolve. Member access through `NativeAssistantStore.transaction` pins the
 * `@agentkit/host` port interfaces (the store aggregate the class
 * implements); `NativeProviderConfig` pins the contract/provider surface
 * that leaked `@sinclair/typebox` schema types.
 */
import {
  bindingId,
  NativeAssistantStore,
  NativeStore,
  type NativeAssistantStoreOptions,
  type NativeProviderConfig,
  type NativeStorePort,
} from "@agentkit/native";

export const identity: string = bindingId;

export function openInMemory(): Promise<NativeStore> {
  return NativeStore.inMemory();
}

export function openFile(path: string): Promise<NativeStore> {
  return NativeStore.open(path);
}

export function makeAssistantStore(
  options: NativeAssistantStoreOptions,
): NativeAssistantStore {
  return new NativeAssistantStore(options);
}

/** Touches every `@agentkit/host` port interface the aggregate exposes —
 * if those declarations stop resolving, this file stops compiling. */
export async function readThroughTransaction(
  store: NativeAssistantStore,
): Promise<number> {
  return store.transaction(async (tx) => {
    void tx.conversations;
    void tx.tasks;
    void tx.proposals;
    void tx.inputs;
    void tx.summaries;
    void tx.providers;
    void tx.settings;
    void tx.outbox;
    void tx.continuations;
    void tx.toolExecutions;
    return 1;
  });
}

/** The contract-typed slice of the public surface (formerly typebox-only
 * through `@agentkit/contracts`). */
export const providerConfig: NativeProviderConfig = {
  protocol: "chat-completions",
  providerId: "typed-consumer",
  connectionId: "typed-consumer",
  model: "typed-consumer-model",
  temperature: 0,
  maxTokens: 16,
};

/** The package-local store port surface, for completeness of the entry. */
export function callThrough(port: NativeStorePort): Promise<number> {
  return port.callStore<number>("ping", {});
}
