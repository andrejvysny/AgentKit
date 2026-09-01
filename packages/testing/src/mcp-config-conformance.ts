// The shared behavioral contract every `McpServerConfigStore` implementation
// must pass — @agentkit/adapters-memory and @agentkit/adapters-sqlite both run
// it, so a host can develop against one and deploy against the other without a
// call site changing.
//
// It is a SEPARATE suite from `describeAssistantStoreConformance` because the
// port is a separate store: an MCP server config shares a transaction with
// nothing, so it is not part of the `AssistantStore` aggregate and cannot be
// graded through a harness that hands one out.
//
// FRAMEWORK-NEUTRAL, same rules as the store suite: no runner import, every
// cross-package import is `import type`, and error assertions match on the
// `code` string rather than on `instanceof` (an adapter and a test that
// resolved two copies of the same error class would otherwise fail on identity).
import type {
  McpServerConfigRecord,
  McpServerConfigStore,
} from "@agentkit/mcp-client";
import {
  expectRejectsWithCode,
  type AssistantStoreConformanceTestApi,
} from "./conformance-support.js";

export interface McpServerConfigStoreConformanceHarness {
  store: McpServerConfigStore;
  /** Releases whatever `create()` opened (a db connection, a temp file). */
  close?: () => void;
}

export interface DescribeMcpServerConfigStoreConformanceOptions {
  /** Name the suite reports under, e.g. `"SqliteMcpServerConfigStore"`. */
  name: string;
  /**
   * A fresh, isolated store per call — built over a clock pinned to
   * {@link MCP_CONFIG_UPDATED_AT}, so the suite can assert that `update`
   * stamps `updatedAt` from the store's clock rather than from the record it
   * patched.
   */
  create: () => Promise<McpServerConfigStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/** What every record the suite writes carries as `createdAt`/`updatedAt`. */
export const MCP_CONFIG_CREATED_AT = "2024-01-01T00:00:00.000Z";
/** What the harness's pinned clock must report; see {@link MCP_CONFIG_CREATED_AT}. */
export const MCP_CONFIG_UPDATED_AT = "2024-01-02T00:00:00.000Z";

const T0 = MCP_CONFIG_CREATED_AT;
const T1 = MCP_CONFIG_UPDATED_AT;

/** A complete record — every optional field populated, so nothing round-trips by luck. */
function fullRecord(
  overrides: Partial<McpServerConfigRecord> = {},
): McpServerConfigRecord {
  return {
    id: "mcp-1",
    alias: "notes",
    transport: {
      kind: "stdio",
      command: "notes-server",
      args: ["--stdio"],
      env: { NOTES_TOKEN: "${token}" },
    },
    secretRefs: { token: "secret/notes-token" },
    enabled: true,
    toolAliases: { list_notes: "list" },
    resilience: { requestTimeoutMs: 1_234 },
    createdAt: T0,
    updatedAt: T0,
    ...overrides,
  };
}

export function describeMcpServerConfigStoreConformance(
  options: DescribeMcpServerConfigStoreConformanceOptions,
): void {
  const { name, create, test } = options;
  const { describe, it, expect } = test;

  /** Runs `fn` against a fresh store and closes it however it opened. */
  async function withStore(
    fn: (store: McpServerConfigStore) => Promise<void>,
  ): Promise<void> {
    const harness = await create();
    try {
      await fn(harness.store);
    } finally {
      harness.close?.();
    }
  }

  describe(`${name} — McpServerConfigStore conformance`, () => {
    it("round-trips every field of a fully populated config", async () => {
      await withStore(async (store) => {
        const record = fullRecord();
        expect(await store.create(record)).toEqual(record);
        expect(await store.get("mcp-1")).toEqual(record);
        expect(await store.list()).toEqual([record]);
      });
    });

    it("keeps an ABSENT `enabled` absent rather than defaulting it into the record", async () => {
      await withStore(async (store) => {
        // `enabled?: boolean` with "absent means true" is a THIRD state, not a
        // synonym for `true`: a store that normalized it would rewrite a
        // record the caller can no longer tell from one that said `true`.
        const record = fullRecord({ id: "mcp-min", alias: "min" });
        delete record.enabled;
        delete record.secretRefs;
        delete record.toolAliases;
        delete record.resilience;
        await store.create(record);
        const read = await store.get("mcp-min");
        expect(read).toEqual(record);
        expect(read !== null && "enabled" in read).toBe(false);
      });
    });

    it("stores `enabled: false` as false, not as absent", async () => {
      await withStore(async (store) => {
        await store.create(fullRecord({ enabled: false }));
        expect((await store.get("mcp-1"))?.enabled).toBe(false);
      });
    });

    it("answers null for an id nothing has, and an empty list for an empty store", async () => {
      await withStore(async (store) => {
        expect(await store.get("nope")).toBeNull();
        expect(await store.list()).toEqual([]);
      });
    });

    it("refuses a duplicate alias, and leaves the first record untouched", async () => {
      await withStore(async (store) => {
        await store.create(fullRecord());
        await expectRejectsWithCode(
          store.create(fullRecord({ id: "mcp-2" })),
          "mcp_invalid_config",
          expect,
        );
        expect((await store.list()).map((row) => row.id)).toEqual(["mcp-1"]);
      });
    });

    it("treats aliases case-SENSITIVELY", async () => {
      await withStore(async (store) => {
        // The alias grammar has no uppercase in it, so a case-insensitive
        // comparison could only ever differ on values that cannot be connected
        // anyway — and would refuse a write for a collision that is not one.
        await store.create(fullRecord());
        await store.create(fullRecord({ id: "mcp-2", alias: "Notes" }));
        expect((await store.list()).length).toBe(2);
      });
    });

    it("refuses a duplicate id", async () => {
      await withStore(async (store) => {
        await store.create(fullRecord());
        await expectRejectsWithCode(
          store.create(fullRecord({ alias: "other" })),
          "mcp_invalid_config",
          expect,
        );
      });
    });

    it("patches only the fields named, replacing nested bags wholesale", async () => {
      await withStore(async (store) => {
        await store.create(fullRecord());
        const updated = await store.update("mcp-1", {
          enabled: false,
          // A field-level REPLACE: the stored `{ token: ... }` map is gone, not
          // merged with. A merge makes "remove this ref" unexpressible.
          secretRefs: { other: "secret/other" },
        });
        expect(updated.enabled).toBe(false);
        expect(updated.secretRefs).toEqual({ other: "secret/other" });
        // Untouched fields survive verbatim, nested ones included.
        expect(updated.alias).toBe("notes");
        expect(updated.transport).toEqual(fullRecord().transport);
        expect(updated.toolAliases).toEqual({ list_notes: "list" });
        expect(updated.createdAt).toBe(T0);
        expect(await store.get("mcp-1")).toEqual(updated);
      });
    });

    it("stamps updatedAt on a patch and never moves createdAt", async () => {
      await withStore(async (store) => {
        await store.create(fullRecord());
        const updated = await store.update("mcp-1", { enabled: false });
        expect(updated.createdAt).toBe(T0);
        expect(updated.updatedAt).toBe(T1);
      });
    });

    it("accepts a rename, and a patch that re-states the record's own alias", async () => {
      await withStore(async (store) => {
        await store.create(fullRecord());
        // An edit form resends every field it read; refusing that would make
        // "save" fail on a rename that never happened.
        expect((await store.update("mcp-1", { alias: "notes" })).alias).toBe(
          "notes",
        );
        expect((await store.update("mcp-1", { alias: "memos" })).alias).toBe(
          "memos",
        );
        // And the alias it left behind is free again.
        await store.create(fullRecord({ id: "mcp-2", alias: "notes" }));
        expect((await store.list()).map((row) => row.alias)).toEqual([
          "memos",
          "notes",
        ]);
      });
    });

    it("refuses a rename onto another record's alias, and rolls the write back", async () => {
      await withStore(async (store) => {
        await store.create(fullRecord());
        await store.create(fullRecord({ id: "mcp-2", alias: "memos" }));
        await expectRejectsWithCode(
          store.update("mcp-2", { alias: "notes", enabled: false }),
          "mcp_invalid_config",
          expect,
        );
        // The whole patch is refused, not just the colliding half.
        const untouched = await store.get("mcp-2");
        expect(untouched?.alias).toBe("memos");
        expect(untouched?.enabled).toBe(true);
      });
    });

    it("raises mcp_config_not_found for an unknown id on update and delete", async () => {
      await withStore(async (store) => {
        await expectRejectsWithCode(
          store.update("nope", { enabled: false }),
          "mcp_config_not_found",
          expect,
        );
        await expectRejectsWithCode(
          store.delete("nope"),
          "mcp_config_not_found",
          expect,
        );
      });
    });

    it("deletes, freeing the alias", async () => {
      await withStore(async (store) => {
        await store.create(fullRecord());
        await store.delete("mcp-1");
        expect(await store.get("mcp-1")).toBeNull();
        expect(await store.list()).toEqual([]);
        await store.create(fullRecord({ id: "mcp-2" }));
        expect((await store.list()).map((row) => row.id)).toEqual(["mcp-2"]);
      });
    });

    it("lists createdAt ascending, whatever order the writes arrived in", async () => {
      await withStore(async (store) => {
        await store.create(
          fullRecord({ id: "late", alias: "late", createdAt: T1 }),
        );
        await store.create(
          fullRecord({ id: "early", alias: "early", createdAt: T0 }),
        );
        expect((await store.list()).map((row) => row.id)).toEqual([
          "early",
          "late",
        ]);
      });
    });

    it("hands out snapshots: mutating what a caller holds cannot edit the store", async () => {
      await withStore(async (store) => {
        const record = fullRecord();
        await store.create(record);
        // The object the caller passed in is still the caller's.
        record.alias = "mutated";
        if (record.secretRefs !== undefined)
          record.secretRefs["token"] = "oops";

        const read = await store.get("mcp-1");
        expect(read?.alias).toBe("notes");
        expect(read?.secretRefs).toEqual({ token: "secret/notes-token" });

        // And so is the one it read back — nested bags included, which a
        // shallow copy would share.
        if (read !== null) {
          read.alias = "mutated-again";
          if (read.secretRefs !== undefined) read.secretRefs["token"] = "oops";
        }
        expect((await store.get("mcp-1"))?.alias).toBe("notes");
        expect((await store.get("mcp-1"))?.secretRefs).toEqual({
          token: "secret/notes-token",
        });
      });
    });
  });
}
