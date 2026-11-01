/**
 * Receipt-bound proposal apply through the ACTUAL native binding.
 *
 * Invariants pinned here (external review at 82ad438):
 *  - A status request must never manufacture appliedOps evidence:
 *    `transition(→applied)` without a recorded outcome is the plain CAS the
 *    TypeScript baseline had, and leaves `getOutcome` null — even after a
 *    fresh-process reopen.
 *  - A committed receipt is first-write-wins, byte-identical, and the
 *    terminal stamp derives from the RECEIPT, never from a replayed argument.
 */
import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { NativeStore } from "../src/index.js";
import { NativeAssistantStore } from "../src/assistant-store.js";
import { available } from "./helpers/binding.js";

const nativeTest = available ? test : test.skip;

function portFor(store: NativeStore): InstanceType<typeof NativeAssistantStore> {
  return new NativeAssistantStore({ store });
}

function proposalInput(id: string, operations: unknown[] = [{ op: "write", path: "a.ts" }]) {
  return {
    id,
    chatId: "chat-1",
    scopeKey: `scope-${id}`,
    toolName: "test.tool",
    kind: "test.kind",
    risk: "low",
    envelope: {},
    operations,
    warnings: [],
    truncated: false,
    createdAt: new Date(1_700_000_000_000).toISOString(),
  } as never;
}

async function toApplying(port: ReturnType<typeof portFor>, id: string, operation: string) {
  await port.proposals.create(proposalInput(id));
  await port.proposals.transition(id, ["pending"], "approved", {
    decision: { actor: "user", decidedAt: new Date().toISOString() },
  });
  await port.proposals.transition(id, ["approved"], "applying", { operationId: operation });
}

nativeTest(
  "transition to applied without a recorded outcome leaves no receipt, even after reopen",
    async () => {
    const directory = mkdtempSync(join(tmpdir(), "agentkit-proposal-evidence-"));
    const path = join(directory, "store.sqlite");
    const store = await NativeStore.open(path);
    const port = portFor(store);
    await toApplying(port, "prp-uncas", "op-uncas");
    const applied = await port.proposals.transition("prp-uncas", ["applying"], "applied", {});
    expect(applied?.status).toBe("applied");
    expect(applied?.claimedAt).toBeDefined(); // claim survives finishing
    expect(await port.proposals.getOutcome("op-uncas")).toBeNull();
    await store.close();
    // Fresh process: no evidence row was ever written.
    const reopened = await NativeStore.open(path);
    const port2 = portFor(reopened);
    expect((await port2.proposals.get("prp-uncas"))?.status).toBe("applied");
    expect(await port2.proposals.getOutcome("op-uncas")).toBeNull();
    // Late evidence still stands on its own.
    const late = await port2.proposals.recordOutcome("op-uncas", {
      status: "partial",
      appliedOps: 1,
      failedOps: [{ opIndex: 1, error: "second op refused" }],
    } as never);
    expect(late.status).toBe("partial");
    expect(await port2.proposals.getOutcome("op-uncas")).not.toBeNull();
    await reopened.close();
  },
  { timeout: 30_000 },
);

nativeTest(
  "a committed receipt is first-write-wins and the stamp derives from it, not the replay",
    async () => {
    const directory = mkdtempSync(join(tmpdir(), "agentkit-proposal-receipt-"));
    const path = join(directory, "store.sqlite");
    const store = await NativeStore.open(path);
    const port = portFor(store);
    await toApplying(port, "prp-partial", "op-partial");
    const partial = {
      status: "partial" as const,
      appliedOps: 1,
      failedOps: [{ opIndex: 1, error: "halfway" }],
    };
    const recorded = await port.proposals.recordOutcome(
      "op-partial",
      { ...partial, resultJson: null, revision: null } as never,
    );
    // The port projects the verdict out of the engine's receipt; absent
    // optional strings are omitted, not nulled (baseline projection).
    expect(recorded).toEqual(partial);
    // The settle goes through the receipt: replaying a DIFFERENT outcome via
    // transition must not restate the evidence.
    const applied = await port.proposals.transition("prp-partial", ["applying"], "applied", {});
    expect(applied?.status).toBe("applied");
    const receipt = await port.proposals.getOutcome("op-partial");
    expect(receipt).toEqual(partial);
    await store.close();
    // Fresh process: still byte-identical, and a restated record is refused.
    const reopened = await NativeStore.open(path);
    const port2 = portFor(reopened);
    const restated = {
      status: "failed",
      appliedOps: 0,
      failedOps: [{ opIndex: 0, error: "rewrite" }],
    };
    const replay = await port2.proposals.recordOutcome("op-partial", restated as never);
    expect(replay).toEqual(partial);
    expect((await port2.proposals.get("prp-partial"))?.status).toBe("applied");
    await reopened.close();
  },
  { timeout: 30_000 },
);
