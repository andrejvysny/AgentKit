import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeStore, type NativeStorePort } from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;
const provider = {
  id: "p",
  label: "Provider",
  kind: "openai-compatible",
  baseUrl: "http://localhost",
  defaultModel: "m",
  enabled: true,
};

nativeTest(
  "native transactions commit, roll back, flatten, and expire their views",
  async () => {
    const store = await NativeStore.inMemory();
    let saved: NativeStorePort | undefined;
    try {
      await store.transaction(async (tx) => {
        saved = tx;
        await tx.callStore("providers.upsert", provider);
        await tx.transaction(async (nested) => {
          await nested.callStore("providers.saveCapabilities", {
            providerId: "p",
            capabilities: {
              streaming: true,
              modelList: false,
              toolCalling: false,
            },
          });
        });
      });
      expect(
        await store.callStore<typeof provider>("providers.get", {
          providerId: "p",
        }),
      ).toEqual(provider);
      await expect(saved!.callStore("providers.list")).rejects.toMatchObject({
        code: "transaction_scope_expired",
      });
      await expect(
        store.transaction(async (tx) => {
          await tx.callStore("providers.delete", { providerId: "p" });
          throw new Error("rollback");
        }),
      ).rejects.toThrow("rollback");
      expect(
        await store.callStore<typeof provider>("providers.get", {
          providerId: "p",
        }),
      ).toEqual(provider);
      expect(
        await store.callStore<{ streaming: boolean }>(
          "providers.getCapabilities",
          { providerId: "p" },
        ),
      ).toMatchObject({ streaming: true });
    } finally {
      await store.close();
    }
  },
);

nativeTest(
  "root access inside a native transaction times out without joining it",
  async () => {
    const store = await NativeStore.inMemory();
    store.setTransactionGateTimeout(20);
    try {
      await store.transaction(async (tx) => {
        await expect(
          store.callStore("providers.upsert", { ...provider, id: "outside" }),
        ).rejects.toMatchObject({ code: "transaction_gate_timeout" });
        await tx.callStore("providers.upsert", provider);
      });
      expect(
        (await store.callStore<(typeof provider)[]>("providers.list")).map(
          (row) => row.id,
        ),
      ).toEqual(["p"]);
    } finally {
      await store.close();
    }
  },
);

nativeTest(
  "settled native transactions release file ownership before close resolves",
  async () => {
    const directory = mkdtempSync(join(tmpdir(), "native-transaction-reopen-"));
    const file = join(directory, "store.sqlite");
    try {
      for (let iteration = 0; iteration < 16; iteration++) {
        const store = await NativeStore.open(file);
        try {
          await store.transaction(async (tx) => {
            await tx.callStore("providers.upsert", provider);
          });
        } finally {
          await store.close();
        }
      }
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  },
);
