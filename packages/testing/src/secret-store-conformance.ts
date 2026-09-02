// The shared behavioral contract every `SecretStore` implementation must
// pass — @agentkit/adapters-memory's `MemorySecretStore` runs it today, and
// any other implementation (an OS keychain, an encrypted file) can develop
// against it too.
//
// FRAMEWORK-NEUTRAL, same rules as the other conformance suites here: no
// runner import, every cross-package import is `import type`, and — since
// `SecretStore` defines no error codes at all (`get`/`delete` on an unknown
// ref are normal, not exceptional) — nothing in this file asserts on a
// thrown `code`.
import type { SecretStore } from "@agentkit/host";
import type { AssistantStoreConformanceTestApi } from "./conformance-support.js";

export interface SecretStoreConformanceHarness {
  store: SecretStore;
  /** Releases whatever `create()` opened (a db connection, a temp file). */
  close?: () => void;
}

export interface DescribeSecretStoreConformanceOptions {
  /** Adapter name, folded into every `describe` block title. */
  name: string;
  /** A fresh, isolated store per call — never shared across `it()`s. */
  create: () => Promise<SecretStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

export function describeSecretStoreConformance(
  options: DescribeSecretStoreConformanceOptions,
): void {
  const { name, create, test } = options;
  const { describe, it, expect } = test;

  /** Runs `fn` against a fresh store and closes it however it opened. */
  async function withStore(
    fn: (store: SecretStore) => Promise<void>,
  ): Promise<void> {
    const harness = await create();
    try {
      await fn(harness.store);
    } finally {
      harness.close?.();
    }
  }

  describe(`${name} — SecretStore conformance`, () => {
    it("answers null for a ref nothing has set — an absent secret is a normal state, not an error", async () => {
      await withStore(async (store) => {
        expect(await store.get("provider/unknown/api-key")).toBeNull();
      });
    });

    it("round-trips a set value through get", async () => {
      await withStore(async (store) => {
        await store.set("provider/p1/api-key", "sk-abc123");
        expect(await store.get("provider/p1/api-key")).toBe("sk-abc123");
      });
    });

    it("a second set for the same ref overwrites the first", async () => {
      await withStore(async (store) => {
        await store.set("provider/p1/api-key", "sk-first");
        await store.set("provider/p1/api-key", "sk-second");
        expect(await store.get("provider/p1/api-key")).toBe("sk-second");
      });
    });

    it("delete removes the value — get answers null and listRefs drops it", async () => {
      await withStore(async (store) => {
        await store.set("provider/p1/api-key", "sk-abc123");
        await store.delete("provider/p1/api-key");
        expect(await store.get("provider/p1/api-key")).toBeNull();
        expect(await store.listRefs()).not.toContain("provider/p1/api-key");
      });
    });

    it("delete of a ref nothing has set does not throw", async () => {
      await withStore(async (store) => {
        await store.delete("provider/never-set/api-key");
        expect(await store.get("provider/never-set/api-key")).toBeNull();
      });
    });

    it("listRefs reports exactly the refs currently set, and none of their values", async () => {
      await withStore(async (store) => {
        expect(await store.listRefs()).toEqual([]);
        await store.set("provider/p1/api-key", "sk-p1");
        await store.set("provider/p2/api-key", "sk-p2");
        const refs = await store.listRefs();
        expect(refs.length).toBe(2);
        expect(refs).toContain("provider/p1/api-key");
        expect(refs).toContain("provider/p2/api-key");
        for (const ref of refs) {
          expect(typeof ref).toBe("string");
        }
      });
    });

    it("refs are opaque strings — `/` and `:` are ordinary characters, not delimiters the store parses", async () => {
      await withStore(async (store) => {
        const ref = "provider/p1:staging/api-key";
        await store.set(ref, "sk-namespaced");
        expect(await store.get(ref)).toBe("sk-namespaced");
        expect(await store.listRefs()).toContain(ref);
      });
    });

    it('an empty string is a value, not an absence — get answers "", never null', async () => {
      await withStore(async (store) => {
        await store.set("provider/p1/api-key", "");
        expect(await store.get("provider/p1/api-key")).toBe("");
        expect(await store.listRefs()).toContain("provider/p1/api-key");
      });
    });

    it("a value with unicode round-trips byte-exact", async () => {
      await withStore(async (store) => {
        const value = "sk-héllo-🔑-日本語";
        await store.set("provider/p1/api-key", value);
        expect(await store.get("provider/p1/api-key")).toBe(value);
      });
    });
  });
}
