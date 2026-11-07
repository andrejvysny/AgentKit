import { testServiceOptions } from "./helpers/test-profiles.js";
import { expect, test } from "bun:test";
import { NativeStore } from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

nativeTest(
  "uncertain execution surface lists, validates and refuses explicitly",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService(testServiceOptions());
    try {
      await service.createSession({ sessionId: "s1", profileId: "p1" });
      expect(await service.listUncertainExecutions("s1")).toEqual([]);
      await expect(
        service.listUncertainExecutions("nope"),
      ).rejects.toMatchObject({ code: "session_not_found" });
      // An unknown receipt never reveals whether it exists elsewhere.
      await expect(
        service.resolveUncertainExecution({
          sessionId: "s1",
          executionId: "missing",
          outcome: "not_started",
        }),
      ).rejects.toMatchObject({ code: "not_found" });
      // A completed outcome needs a result; others forbid one.
      await expect(
        service.resolveUncertainExecution({
          sessionId: "s1",
          executionId: "missing",
          outcome: "completed",
        }),
      ).rejects.toMatchObject({ code: "invalid_input" });
      await expect(
        service.resolveUncertainExecution({
          sessionId: "s1",
          executionId: "missing",
          outcome: "acknowledged",
          result: "x",
        }),
      ).rejects.toMatchObject({ code: "invalid_input" });
      await service.close();
      await expect(service.listUncertainExecutions("s1")).rejects.toMatchObject(
        { code: "closed" },
      );
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);
