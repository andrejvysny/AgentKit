import { expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadBinding } from "../src/loader.js";
import type { NativeStore as RawNativeStore } from "../generated/index.js";
import { available, missing, required } from "./helpers/binding.js";

/**
 * AGENTKIT-90 M1: the persistent-teammate lane through the raw bindings
 * (`NativeStore.createTeammates` → `NativeTeammates`). Service-level cases
 * mirror `rust/crates/host/tests/http_teammates.rs` semantics at the
 * binding boundary: the lifecycle + revision ledger walk, stale-revision
 * conflict, archived admission refusal, duplicate-admission replay,
 * cross-scope list isolation, unknown-usage-as-unknown projections,
 * credential redaction on event pages, queued cancellation, projection
 * identity across a store reopen, and the sealed run-profile explanation.
 * JSON crosses the boundary as strings; errors carry contract codes.
 */

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

async function openStore(path?: string): Promise<RawNativeStore> {
  const binding = await loadBinding();
  return path === undefined
    ? binding.NativeStore.memory()
    : binding.NativeStore.open(path);
}

/** JSON payload in, parsed JSON out — the lane speaks JSON strings. */
async function call<T = unknown>(promise: Promise<string | null>): Promise<T> {
  return JSON.parse((await promise) as string) as T;
}

/** A raw binding rejection carries `{code, message, details?}` in its
 * `Error.message`; assert the stable code and hand back the payload. */
async function expectCode(
  promise: Promise<unknown>,
  code: string,
): Promise<{ code: string; message: string }> {
  try {
    await promise;
  } catch (error) {
    const payload = JSON.parse((error as Error).message) as {
      code: string;
      message: string;
    };
    expect(payload.code).toBe(code);
    return payload;
  }
  throw new Error(`expected a rejection with code ${code}`);
}

function definition(name: string): Record<string, unknown> {
  return {
    name,
    purpose: `purpose of ${name}`,
    instructions: `instructions of ${name}`,
    permissionMode: "ask",
    allowedTools: ["read_file"],
  };
}

interface Admitted {
  task: {
    taskId: string;
    status: string;
    runProfile?: { digest: string };
  };
  replayed: boolean;
}

function submitBody(agentId: string, key: string): string {
  return JSON.stringify({
    agentId,
    admissionKey: key,
    kind: "review.diff",
    payload: { marker: key },
  });
}

/** The durable event envelope the store accepts, mirroring the host
 * tests' envelope helper. */
function envelope(
  seq: number,
  type: string,
  extra: Record<string, unknown>,
): Record<string, unknown> {
  return {
    type,
    seq,
    eventId: `evt-${seq}`,
    timestamp: "2026-11-07T12:00:00.000Z",
    contractVersion: "0.7.0",
    ...extra,
  };
}

/** Claim the task and append durable events exactly as a worker would. */
async function emitEvents(
  store: RawNativeStore,
  taskId: string,
  events: Record<string, unknown>[],
): Promise<void> {
  const claimed = JSON.parse(
    await store.callStore(
      "tasks.claimNext",
      JSON.stringify({
        input: {
          ownerId: "worker",
          now: Date.now(),
          ttlMs: 600_000,
          kinds: [],
          scopesBusy: [],
          maxAttempts: 3,
        },
      }),
    ),
  ) as { task: { taskId: string }; lease: { leaseToken: string } };
  expect(claimed.task.taskId).toBe(taskId);
  await store.callStore(
    "tasks.appendEvents",
    JSON.stringify({
      taskId,
      events,
      opts: { leaseToken: claimed.lease.leaseToken },
      now: Date.now(),
    }),
  );
}

/** Seed the `native` provider the run-profile resolver resolves to. */
async function seedNativeProvider(store: RawNativeStore): Promise<void> {
  await store.callStore(
    "providers.upsert",
    JSON.stringify({
      id: "native",
      label: "native",
      kind: "native",
      baseUrl: "http://127.0.0.1:9",
      defaultModel: "model",
      enabled: true,
    }),
  );
  await store.callStore(
    "providers.saveCapabilities",
    JSON.stringify({
      providerId: "native",
      capabilities: { streaming: true, toolCalling: true, modelList: false },
    }),
  );
  await store.callStore(
    "providers.replaceModels",
    JSON.stringify({
      providerId: "native",
      models: [
        {
          providerId: "native",
          modelId: "model",
          displayName: "model",
          fetchedAt: "2026-01-01T00:00:00.000Z",
          contextWindowTokens: 128_000,
          supportsToolCalling: true,
        },
      ],
    }),
  );
}

nativeTest(
  "lifecycle and the revision ledger walk through the bindings",
  async () => {
    const store = await openStore();
    const lane = await store.createTeammates(null);
    const created = await call<{
      agentId: string;
      currentRevision: number;
      lifecycle: string;
      scope: string;
      ownerId: string;
    }>(
      lane.createAgent("scope-a", "owner-1", JSON.stringify(definition("rev"))),
    );
    expect(created.currentRevision).toBe(1);
    expect(created.lifecycle).toBe("active");
    expect(created.scope).toBe("scope-a");
    expect(created.ownerId).toBe("owner-1");

    const updated = await call<{
      currentRevision: number;
      definition: { instructions: string };
    }>(
      lane.updateAgent(
        created.agentId,
        1,
        JSON.stringify({ ...definition("rev"), instructions: "tighter" }),
      ),
    );
    expect(updated.currentRevision).toBe(2);
    expect(updated.definition.instructions).toBe("tighter");

    const modeled = await call<{
      currentRevision: number;
      definition: { model: string };
      agentId: string;
    }>(lane.setAgentModel(created.agentId, 2, "model", undefined));
    expect(modeled.currentRevision).toBe(3);
    expect(modeled.definition.model).toBe("model");
    expect(modeled.agentId).toBe(created.agentId);

    const archived = await call<{ lifecycle: string }>(
      lane.archiveAgent(created.agentId),
    );
    expect(archived.lifecycle).toBe("archived");
    const restored = await call<{ lifecycle: string; currentRevision: number }>(
      lane.restoreAgent(created.agentId),
    );
    expect(restored.lifecycle).toBe("active");
    expect(restored.currentRevision).toBe(4);

    const revisions = await call<{ revision: number; source: string }[]>(
      lane.listAgentRevisions(created.agentId),
    );
    expect(revisions.map((entry) => entry.source)).toEqual([
      "create",
      "edit",
      "model_change",
      "restore",
    ]);
    expect(revisions.map((entry) => entry.revision)).toEqual([1, 2, 3, 4]);
    lane.close();
    await store.close();
  },
);

nativeTest(
  "a stale expectedRevision conflicts and applies nothing",
  async () => {
    const store = await openStore();
    const lane = await store.createTeammates(null);
    const created = await call<{ agentId: string }>(
      lane.createAgent(
        "scope-a",
        "owner-1",
        JSON.stringify(definition("stale")),
      ),
    );
    await lane.updateAgent(
      created.agentId,
      1,
      JSON.stringify(definition("stale-v2")),
    );
    await expectCode(
      lane.updateAgent(
        created.agentId,
        1,
        JSON.stringify(definition("stale-v3")),
      ),
      "revision_conflict",
    );
    const current = await call<{
      currentRevision: number;
      definition: { name: string };
    }>(lane.getAgent(created.agentId));
    expect(current.currentRevision).toBe(2);
    expect(current.definition.name).toBe("stale-v2");
    // Unknown agents are absence, with the stable code.
    await expectCode(lane.getAgent("agent-missing"), "not_found");
    lane.close();
    await store.close();
  },
);

nativeTest(
  "an archived agent refuses admission while history stays readable",
  async () => {
    const store = await openStore();
    const lane = await store.createTeammates(null);
    const created = await call<{ agentId: string }>(
      lane.createAgent(
        "scope-a",
        "owner-1",
        JSON.stringify(definition("arch")),
      ),
    );
    const admitted = await call<Admitted>(
      lane.submitTeammateTask(submitBody(created.agentId, "k1")),
    );
    expect(admitted.task.status).toBe("queued");
    await lane.archiveAgent(created.agentId);
    await expectCode(
      lane.submitTeammateTask(submitBody(created.agentId, "k2")),
      "invalid_task_transition",
    );
    // The refusal narrows admission only: the earlier task stays readable.
    const projection = await call<{ task: { status: string; taskId: string } }>(
      lane.getAgentTaskProjection(admitted.task.taskId, null),
    );
    expect(projection.task.taskId).toBe(admitted.task.taskId);
    lane.close();
    await store.close();
  },
);

nativeTest(
  "a duplicate admissionKey replays the same logical task",
  async () => {
    const store = await openStore();
    const lane = await store.createTeammates(null);
    const created = await call<{ agentId: string }>(
      lane.createAgent(
        "scope-a",
        "owner-1",
        JSON.stringify(definition("replay")),
      ),
    );
    const first = await call<Admitted>(
      lane.submitTeammateTask(submitBody(created.agentId, "same-key")),
    );
    expect(first.replayed).toBe(false);
    const second = await call<Admitted>(
      lane.submitTeammateTask(submitBody(created.agentId, "same-key")),
    );
    expect(second.replayed).toBe(true);
    expect(second.task.taskId).toBe(first.task.taskId);
    const tasks = await call<{ taskId: string }[]>(
      lane.listAgentTasks(created.agentId),
    );
    expect(tasks.map((task) => task.taskId)).toEqual([first.task.taskId]);
    lane.close();
    await store.close();
  },
);

nativeTest(
  "agent listings isolate scopes: other scopes are absence",
  async () => {
    const store = await openStore();
    const lane = await store.createTeammates(null);
    for (const name of ["a1", "a2"])
      await lane.createAgent(
        "scope-a",
        "owner-1",
        JSON.stringify(definition(name)),
      );
    await lane.createAgent(
      "scope-b",
      "owner-1",
      JSON.stringify(definition("b1")),
    );
    const inA = await call<{ agentId: string; scope: string }[]>(
      lane.listAgents("scope-a"),
    );
    const inB = await call<{ agentId: string; scope: string }[]>(
      lane.listAgents("scope-b"),
    );
    const inC = await call<unknown[]>(lane.listAgents("scope-c"));
    expect(inA).toHaveLength(2);
    expect(inB).toHaveLength(1);
    expect(inC).toEqual([]);
    expect(inA.every((agent) => agent.scope === "scope-a")).toBe(true);
    expect(inB[0]?.scope).toBe("scope-b");
    lane.close();
    await store.close();
  },
);

nativeTest(
  "the projection reports unknown usage as unknown, never a zero",
  async () => {
    const store = await openStore();
    const lane = await store.createTeammates(null);
    const created = await call<{ agentId: string }>(
      lane.createAgent(
        "scope-a",
        "owner-1",
        JSON.stringify(definition("usage")),
      ),
    );
    const admitted = await call<Admitted>(
      lane.submitTeammateTask(submitBody(created.agentId, "k1")),
    );
    const projection = await call<{
      eventCount: number;
      usage: Record<string, number | null>;
    }>(lane.getAgentTaskProjection(admitted.task.taskId, null));
    expect(projection.eventCount).toBe(0);
    expect(projection.usage.promptTokens).toBeNull();
    expect(projection.usage.completionTokens).toBeNull();
    expect(projection.usage.totalTokens).toBeNull();
    expect(projection.usage.usageEvents).toBe(0);
    lane.close();
    await store.close();
  },
);

nativeTest(
  "event pages redact credentials and oversized tool text",
  async () => {
    const store = await openStore();
    const lane = await store.createTeammates(null);
    const created = await call<{ agentId: string }>(
      lane.createAgent(
        "scope-a",
        "owner-1",
        JSON.stringify(definition("leaky")),
      ),
    );
    const admitted = await call<Admitted>(
      lane.submitTeammateTask(submitBody(created.agentId, "k1")),
    );
    const huge = "x".repeat(20_000);
    await emitEvents(store, admitted.task.taskId, [
      envelope(1, "run.tool.request", {
        toolName: "http.fetch",
        arguments: {
          apiKey: "SENTINEL-KEY",
          headers: { authorization: "Bearer SENTINEL" },
        },
      }),
      envelope(2, "run.tool.result", { toolName: "http.fetch", result: huge }),
    ]);
    const page = await call<{ events: Record<string, unknown>[] }>(
      lane.listAgentTaskEvents(admitted.task.taskId, null, 50),
    );
    expect(page.events).toHaveLength(2);
    const text = JSON.stringify(page.events);
    expect(text.includes("SENTINEL-KEY")).toBe(false);
    expect(text.includes("Bearer SENTINEL")).toBe(false);
    expect(text.includes(huge)).toBe(false);
    const first = page.events[0] as {
      arguments: { apiKey: string; headers: { authorization: string } };
      agentId: string;
      seq: number;
    };
    expect(first.arguments.apiKey).toBe("***");
    expect(first.arguments.headers.authorization).toBe("***");
    // Identity and ordering fields survive redaction untouched.
    expect(first.agentId).toBe(created.agentId);
    expect(first.seq).toBe(1);
    const second = page.events[1] as { result: Record<string, unknown> };
    expect(second.result.truncated).toBe(true);
    expect(second.result.bytes).toBe(20_000);
    // Redaction narrows only the boundary view: the durable log is intact.
    const durable = await store.callStore(
      "tasks.listEvents",
      JSON.stringify({ taskId: admitted.task.taskId, opts: {} }),
    );
    expect(durable.includes("SENTINEL-KEY")).toBe(true);
    lane.close();
    await store.close();
  },
);

nativeTest("cancel settles a queued task and is idempotent", async () => {
  const store = await openStore();
  const lane = await store.createTeammates(null);
  const created = await call<{ agentId: string }>(
    lane.createAgent(
      "scope-a",
      "owner-1",
      JSON.stringify(definition("cancel")),
    ),
  );
  const admitted = await call<Admitted>(
    lane.submitTeammateTask(submitBody(created.agentId, "k1")),
  );
  const cancelled = await call<{ status: string; cancelRequested: boolean }>(
    lane.cancelAgentTask(admitted.task.taskId),
  );
  expect(cancelled.status).toBe("cancelled");
  expect(cancelled.cancelRequested).toBe(true);
  const again = await call<{ status: string }>(
    lane.cancelAgentTask(admitted.task.taskId),
  );
  expect(again.status).toBe("cancelled");
  const projection = await call<{ task: { status: string } }>(
    lane.getAgentTaskProjection(admitted.task.taskId, null),
  );
  expect(projection.task.status).toBe("cancelled");
  lane.close();
  await store.close();
});

nativeTest(
  "the projection reconstructs identically after a store reopen",
  async () => {
    const directory = mkdtempSync(join(tmpdir(), "native-teammates-"));
    const path = join(directory, "state.sqlite");
    try {
      let store = await openStore(path);
      let lane = await store.createTeammates(null);
      const created = await call<{ agentId: string }>(
        lane.createAgent(
          "scope-a",
          "owner-1",
          JSON.stringify(definition("durable")),
        ),
      );
      const admitted = await call<Admitted>(
        lane.submitTeammateTask(submitBody(created.agentId, "k1")),
      );
      await emitEvents(store, admitted.task.taskId, [
        envelope(1, "run.note", { note: "kept" }),
        envelope(2, "run.usage", {
          usage: { promptTokens: 7, completionTokens: 3 },
        }),
      ]);
      const before = await call<Record<string, unknown>>(
        lane.getAgentTaskProjection(admitted.task.taskId, null),
      );
      expect(before.lastSeq).toBe(2);
      const usage = before.usage as Record<string, number | null>;
      expect(usage.promptTokens).toBe(7);
      expect(usage.completionTokens).toBe(3);
      // Total is only ever a reported number: never prompt+completion.
      expect(usage.totalTokens).toBeNull();
      expect(usage.usageEvents).toBe(1);
      lane.close();
      await store.close();

      // A fresh handle over the SAME file: the projection is derived, so
      // it reconstructs byte-identically — same canonical status, same
      // bounded event-window views.
      store = await openStore(path);
      lane = await store.createTeammates(null);
      const after = await call<Record<string, unknown>>(
        lane.getAgentTaskProjection(admitted.task.taskId, null),
      );
      expect(after).toEqual(before);
      lane.close();
      await store.close();
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  },
);

nativeTest(
  "submit pins the effective profile and the explanation agrees",
  async () => {
    const store = await openStore();
    await seedNativeProvider(store);
    const lane = await store.createTeammates(null);
    // The model preference rides the definition: profile resolution reads
    // it as the agent source.
    const created = await call<{ agentId: string }>(
      lane.createAgent(
        "scope-a",
        "owner-1",
        JSON.stringify({ ...definition("pinned"), model: "model" }),
      ),
    );
    const pinned = await call<Admitted>(
      lane.submitTeammateTask(
        JSON.stringify({
          agentId: created.agentId,
          admissionKey: "k1",
          kind: "review.diff",
          payload: {},
          taskProfile: { budgets: { maxToolIterations: 4 } },
        }),
      ),
    );
    const digest = pinned.task.runProfile?.digest as string;
    expect(typeof digest).toBe("string");
    expect(digest).toHaveLength(64);
    const explained = await call<{
      taskId: string;
      profileDigest: string;
      agentId: string;
    }>(lane.getAgentTaskRunProfile(pinned.task.taskId));
    expect(explained.taskId).toBe(pinned.task.taskId);
    expect(explained.profileDigest).toBe(digest);
    expect(explained.agentId).toBe(created.agentId);

    // A later definition edit never rewrites the pin.
    await lane.updateAgent(
      created.agentId,
      1,
      JSON.stringify({ ...definition("pinned-again"), model: "other" }),
    );
    const afterEdit = await call<{ profileDigest: string }>(
      lane.getAgentTaskRunProfile(pinned.task.taskId),
    );
    expect(afterEdit.profileDigest).toBe(digest);

    // A task admitted WITHOUT a profile answers with honest absence.
    const bare = await call<Admitted>(
      lane.submitTeammateTask(submitBody(created.agentId, "bare")),
    );
    expect(bare.task.runProfile).toBeUndefined();
    expect(await lane.getAgentTaskRunProfile(bare.task.taskId)).toBeNull();
    lane.close();
    await store.close();
  },
);
