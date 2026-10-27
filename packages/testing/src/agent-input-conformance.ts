// Shared behavioral contract for the optional AssistantStore.inputs port
// (ADR 0016 §2). Framework-neutral like the other suites: test primitives are
// injected, every @agentkit/host import is a type, and errors are matched on
// `code`.
import type { AgentInputStore, CreateAgentInput } from "@agentkit/host";
import {
  expectRejects,
  expectRejectsWithCode,
  type DescribeAssistantStoreConformanceOptions,
} from "./conformance-support.js";

export type DescribeAgentInputStoreConformanceOptions = Pick<
  DescribeAssistantStoreConformanceOptions,
  "create" | "test"
> & { name: string };

export function describeAgentInputStoreConformance(
  options: DescribeAgentInputStoreConformanceOptions,
): void {
  const { name, create, test } = options;
  const { describe, it, expect } = test;

  let counter = 0;
  const uniqueId = (prefix: string): string => {
    counter += 1;
    return `${prefix}-${counter}`;
  };

  const steer = (
    overrides: Partial<CreateAgentInput> = {},
  ): CreateAgentInput => ({
    inputId: uniqueId("input"),
    sessionId: "chat-1",
    kind: "steer",
    content: { parts: [{ type: "text", text: "hello" }] },
    contentHash: "hash-a",
    ...overrides,
  });

  const humanRequest = (
    overrides: Partial<CreateAgentInput> = {},
  ): CreateAgentInput => ({
    inputId: uniqueId("input"),
    sessionId: "chat-1",
    kind: "human_request",
    content: { question: "which?" },
    contentHash: "hash-h",
    attemptId: "attempt-1",
    toolCallId: "call-1",
    expiresAt: "2030-01-01T00:00:00.000Z",
    ...overrides,
  });

  /** Runs `fn` with a fresh store's inputs port, failing loudly if it is absent. */
  const withStore = async (
    fn: (
      inputs: AgentInputStore,
      harness: Awaited<ReturnType<typeof create>>,
    ) => Promise<void>,
  ): Promise<void> => {
    const harness = await create();
    try {
      const inputs = harness.store.inputs;
      expect(inputs).toBeDefined();
      if (!inputs) return;
      await fn(inputs, harness);
    } finally {
      harness.close?.();
    }
  };

  describe(`AgentInputStore conformance — ${name}`, () => {
    it("creates and reads back an input", async () => {
      await withStore(async (inputs) => {
        const input = steer({ targetTaskId: "task-1", onSettled: "cancel" });
        const { record, created } = await inputs.createInput(input);
        expect(created).toBe(true);
        expect(record.status).toBe("pending");
        expect(record.order).toBeGreaterThan(0);
        expect(record.content).toEqual(input.content);
        expect(record.targetTaskId).toBe("task-1");
        expect(record.onSettled).toBe("cancel");
        expect(record.createdAt).toBe(record.updatedAt);
        expect(await inputs.getInput(input.inputId)).toEqual(record);
        expect(await inputs.getInput("missing")).toBeUndefined();

        const request = await inputs.createInput(humanRequest());
        expect(request.record.attemptId).toBe("attempt-1");
        expect(request.record.toolCallId).toBe("call-1");
        expect(request.record.expiresAt).toBe("2030-01-01T00:00:00.000Z");
      });
    });

    it("re-create with the same identity is idempotent", async () => {
      await withStore(async (inputs) => {
        const input = steer();
        const first = await inputs.createInput(input);
        const again = await inputs.createInput(input);
        expect(again.created).toBe(false);
        expect(again.record).toEqual(first.record);
        expect((await inputs.listInputs({ sessionId: "chat-1" })).length).toBe(
          1,
        );
      });
    });

    it("re-create with a different hash, kind or session is input_conflict", async () => {
      await withStore(async (inputs) => {
        const input = steer();
        await inputs.createInput(input);
        for (const changed of [
          { contentHash: "hash-b" },
          { kind: "follow_up" as const },
          { sessionId: "chat-2" },
        ]) {
          await expectRejectsWithCode(
            inputs.createInput({ ...input, ...changed }),
            "input_conflict",
            expect,
          );
        }
        expect((await inputs.getInput(input.inputId))?.contentHash).toBe(
          "hash-a",
        );
      });
    });

    it("rejects malformed creates with invalid_input", async () => {
      await withStore(async (inputs) => {
        const bad: CreateAgentInput[] = [
          steer({ inputId: "" }),
          steer({ sessionId: "" }),
          steer({ attemptId: "a" }),
          { ...steer(), kind: "follow_up", onSettled: "cancel" },
          humanRequest({ attemptId: undefined as unknown as string }),
          humanRequest({ toolCallId: "" }),
          humanRequest({ expiresAt: "soon" }),
        ];
        for (const input of bad) {
          await expectRejectsWithCode(
            inputs.createInput(input),
            "invalid_input",
            expect,
          );
        }
      });
    });

    it("lists by filter in insertion (FIFO) order", async () => {
      await withStore(async (inputs) => {
        const a = (await inputs.createInput(steer({ targetTaskId: "t1" })))
          .record;
        const b = (
          await inputs.createInput(
            steer({ kind: "follow_up", content: "later", contentHash: "h2" }),
          )
        ).record;
        const c = (await inputs.createInput(steer({ targetTaskId: "t2" })))
          .record;
        const other = (await inputs.createInput(steer({ sessionId: "chat-2" })))
          .record;
        expect(a.order < b.order && b.order < c.order).toBe(true);
        expect(other.order > c.order).toBe(true);

        const ids = async (
          query: Parameters<AgentInputStore["listInputs"]>[0],
        ): Promise<string[]> =>
          (await inputs.listInputs(query)).map((record) => record.inputId);

        expect(await ids({ sessionId: "chat-1" })).toEqual([
          a.inputId,
          b.inputId,
          c.inputId,
        ]);
        expect(await ids({ sessionId: "chat-2" })).toEqual([other.inputId]);
        expect(
          await ids({ sessionId: "chat-1", kinds: ["follow_up"] }),
        ).toEqual([b.inputId]);
        expect(await ids({ sessionId: "chat-1", targetTaskId: "t2" })).toEqual([
          c.inputId,
        ]);
        await inputs.transitionInput(a.inputId, ["pending"], "applied");
        expect(
          await ids({ sessionId: "chat-1", statuses: ["pending"] }),
        ).toEqual([b.inputId, c.inputId]);
        expect(
          await ids({
            sessionId: "chat-1",
            kinds: ["steer"],
            statuses: ["applied", "pending"],
          }),
        ).toEqual([a.inputId, c.inputId]);
        expect(await ids({ sessionId: "chat-1", statuses: [] })).toEqual([]);
      });
    });

    it("CAS success writes status, receipt fields and updatedAt", async () => {
      await withStore(async (inputs) => {
        const steered = (await inputs.createInput(steer())).record;
        const applied = await inputs.transitionInput(
          steered.inputId,
          ["pending"],
          "applied",
          { appliedTaskId: "task-9", appliedMessageId: "msg-9" },
        );
        expect(applied.status).toBe("applied");
        expect(applied.appliedTaskId).toBe("task-9");
        expect(applied.appliedMessageId).toBe("msg-9");
        expect(await inputs.getInput(steered.inputId)).toEqual(applied);

        const request = (await inputs.createInput(humanRequest())).record;
        const answered = await inputs.transitionInput(
          request.inputId,
          ["pending"],
          "answered",
          { answerId: "answer-1" },
        );
        expect(answered.status).toBe("answered");
        expect(answered.answerId).toBe("answer-1");
      });
    });

    it("CAS from the wrong status throws and leaves the record unchanged", async () => {
      await withStore(async (inputs) => {
        const { record } = await inputs.createInput(steer());
        await expectRejectsWithCode(
          inputs.transitionInput(record.inputId, ["applied"], "cancelled", {
            appliedTaskId: "nope",
          }),
          "input_transition_invalid",
          expect,
        );
        expect(await inputs.getInput(record.inputId)).toEqual(record);
        await expectRejects(
          inputs.transitionInput("missing", ["pending"], "cancelled"),
          expect,
        );
      });
    });

    it("refuses edges the kind forbids", async () => {
      await withStore(async (inputs) => {
        const cases: [CreateAgentInput, "applied" | "answered"][] = [
          [steer({ kind: "follow_up" }), "applied"],
          [humanRequest(), "applied"],
          [steer(), "answered"],
        ];
        for (const [input, to] of cases) {
          const { record } = await inputs.createInput(input);
          await expectRejectsWithCode(
            inputs.transitionInput(record.inputId, ["pending"], to),
            "input_transition_invalid",
            expect,
          );
          expect((await inputs.getInput(record.inputId))?.status).toBe(
            "pending",
          );
        }
      });
    });

    it("terminal statuses cannot move", async () => {
      await withStore(async (inputs) => {
        const { record } = await inputs.createInput(steer());
        await inputs.transitionInput(record.inputId, ["pending"], "converted", {
          appliedTaskId: "task-2",
        });
        const settled = await inputs.getInput(record.inputId);
        for (const to of ["applied", "cancelled", "pending"] as const) {
          await expectRejectsWithCode(
            inputs.transitionInput(
              record.inputId,
              ["pending", "applied", "converted", "answered", "cancelled"],
              to,
            ),
            "input_transition_invalid",
            expect,
          );
        }
        expect(await inputs.getInput(record.inputId)).toEqual(settled);
      });
    });

    it("deleteSessionInputs removes only that session's inputs and counts them", async () => {
      await withStore(async (inputs) => {
        const a = steer({ sessionId: "chat-del" });
        const b = humanRequest({ sessionId: "chat-del" });
        const other = steer({ sessionId: "chat-keep" });
        for (const input of [a, b, other]) await inputs.createInput(input);
        await inputs.transitionInput(a.inputId, ["pending"], "cancelled");
        expect(await inputs.deleteSessionInputs("chat-del")).toBe(2);
        expect(await inputs.getInput(a.inputId)).toBeUndefined();
        expect(await inputs.getInput(b.inputId)).toBeUndefined();
        expect((await inputs.getInput(other.inputId))?.status).toBe("pending");
        expect(await inputs.deleteSessionInputs("chat-del")).toBe(0);
      });
    });

    it("transaction writes are visible to tx.inputs, then to store.inputs after commit", async () => {
      await withStore(async (inputs, { store }) => {
        const input = steer();
        await store.transaction(async (tx) => {
          expect(tx.inputs).toBeDefined();
          await tx.inputs?.createInput(input);
          expect(await tx.inputs?.getInput(input.inputId)).toBeDefined();
          expect(
            (await tx.inputs?.listInputs({ sessionId: "chat-1" }))?.length,
          ).toBe(1);
        });
        expect((await inputs.getInput(input.inputId))?.status).toBe("pending");
      });
    });

    it("a throw inside transaction() leaves no row", async () => {
      await withStore(async (inputs, { store, capabilities }) => {
        // Adapters without rollback (memory) opt out, as the other suites do.
        if (capabilities?.atomicTransactions === false) return;
        const input = steer();
        await expectRejects(
          store.transaction(async (tx) => {
            await tx.inputs?.createInput(input);
            throw new Error("boom");
          }),
          expect,
        );
        expect(await inputs.getInput(input.inputId)).toBeUndefined();
      });
    });
  });
}
