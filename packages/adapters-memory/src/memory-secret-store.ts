/**
 * Map-backed {@link SecretStore} — the reference implementation of the
 * credential port, and the one a test or a local dev host wires.
 *
 * PROCESS-LIFETIME ONLY: values live in this object's `Map` and nowhere
 * else. Nothing here touches disk, and every value is gone the moment the
 * process (or this instance) is. A real deployment swaps this for the OS
 * keychain, an encrypted file, or a secrets manager — anything implementing
 * the four methods below.
 *
 * STANDALONE, not a seventh member of `MemoryAssistantStore`. `SecretStore`
 * is not part of the `AssistantStore` aggregate — a secret ref shares a
 * transaction with nothing — so it is constructed beside the store, not
 * inside it, the same way `MemoryMcpServerConfigStore` is.
 *
 * Graded by `@agentkit/testing`'s `describeSecretStoreConformance`, run in
 * `packages/adapters-memory/tests/`.
 */
import type { SecretStore } from "@agentkit/host";

export class MemorySecretStore implements SecretStore {
  /**
   * Public, not private: several test suites across this repo assert on
   * exactly what went into this map (and nothing else) — see
   * `packages/react/tests/providers.test.ts`. Exposing it here means those
   * suites can use this reference implementation instead of hand-rolling
   * their own copy.
   */
  readonly values = new Map<string, string>();

  async get(ref: string): Promise<string | null> {
    return this.values.get(ref) ?? null;
  }

  async set(ref: string, value: string): Promise<void> {
    this.values.set(ref, value);
  }

  async delete(ref: string): Promise<void> {
    this.values.delete(ref);
  }

  async listRefs(): Promise<string[]> {
    return [...this.values.keys()];
  }
}
