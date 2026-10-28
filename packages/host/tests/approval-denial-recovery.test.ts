import { afterEach, expect, it } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiChatMessage } from "@agentkit/contracts";
import {
  open,
  drive,
  start,
  pending,
  resume,
  type State,
} from "./fixtures/approval-fixture.js";
import { recordEffect } from "./fixtures/approval-sentinel.js";
import { providerHumanAnswerContent } from "../src/sessions/approval-denial.js";
import type { AssistantStore } from "../src/index.js";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0))
    rmSync(dir, { recursive: true, force: true });
});
type Fixture = Awaited<ReturnType<typeof open>>;
interface Denial {
  summary: string;
  data: {
    errorCode: string;
    errorMessage: string;
    note?: string;
    deniedScope?: string;
    priorExecutionOutcome?: string;
    approvalGeneration?: string;
  };
}

function location() {
  const dir = mkdtempSync(join(tmpdir(), "uncertainty-denial-"));
  dirs.push(dir);
  return {
    database: join(dir, "store.db"),
    sentinel: join(dir, "effects.jsonl"),
  };
}

function observe(f: Fixture): AiChatMessage[][] {
  const requests: AiChatMessage[][] = [];
  const stream = f.mock.streamChat.bind(f.mock);
  f.mock.streamChat = (input) => {
    requests.push(structuredClone(input.messages));
    return stream(input);
  };
  return requests;
}

async function uncertain(writerRoot?: string) {
  const paths = location();
  const state: State = { effects: 0 };
  const options = {
    afterEffect: async (ctx: Parameters<typeof recordEffect>[1]) =>
      recordEffect(paths.sentinel, ctx),
  };
  let f = await open(paths.database, state, writerRoot, options);
  const runId = await start(f);
  await f.service.approve((await pending(f)).inputId, "allow-once");
  state.crash = "after_effect";
  await expect(drive(f)).rejects.toThrow();
  f.store.close();
  f = await open(paths.database, state, writerRoot, options);
  await resume(f, runId);
  return { ...paths, state, options, f, runId, fresh: await pending(f) };
}

function expectRetryFacts(
  content: string,
  generation: string,
  note?: string,
): void {
  const envelope = JSON.parse(content) as Denial;
  expect(envelope.data).toMatchObject({
    errorCode: "approval_denied",
    deniedScope: "uncertainty_retry",
    priorExecutionOutcome: "unknown",
    approvalGeneration: generation,
  });
  expect(envelope.summary).toContain("This retry was not executed.");
  expect(envelope.summary).toContain(
    "earlier execution may already have produced effects",
  );
  expect(envelope.data.errorMessage).toContain("its outcome remains unknown");
  if (note !== undefined) {
    expect(envelope.data.note).toBe(note);
    expect(envelope.data.errorMessage).toContain(note);
  }
}

it("ordinary approval denial retains its original envelope and never invokes the tool", async () => {
  const paths = location();
  const state = { effects: 0 };
  const f = await open(paths.database, state);
  await start(f);
  const request = await pending(f);
  const receipt = await f.service.deny(request.inputId, "deny", "not now");
  const record = (await f.store.conversations.getMessage(receipt.messageId))!;
  const envelope = JSON.parse(String(record.content)) as Denial;
  expect(envelope.summary).toBe("The user denied this tool call.");
  expect(envelope.data.errorMessage).toBe("not now");
  expect(envelope.data.deniedScope).toBeUndefined();
  const requests = observe(f);
  await drive(f);
  expect(
    requests.flat().find((message) => message.role === "tool")?.content,
  ).toBe(record.content);
  expect(state.effects).toBe(0);
  f.store.close();
});

it("fresh retry denial preserves uncertainty with a note, duplicate answer and restart", async () => {
  const prepared = await uncertain();
  let { f } = prepared;
  const note = "Do not retry; I have not inspected the workspace.";
  const receipt = await f.service.deny(
    prepared.fresh.inputId,
    "deny-retry",
    note,
  );
  const stored = (await f.store.conversations.getMessage(receipt.messageId))!;
  const generation = (prepared.fresh.content as { approvalGeneration: string })
    .approvalGeneration;
  expectRetryFacts(String(stored.content), generation, note);
  expect(stored.modelResultJson).toBe(String(stored.content));
  expect(typeof stored.metadata["responseHash"]).toBe("string");
  const duplicate = await f.service.deny(
    prepared.fresh.inputId,
    "deny-retry",
    note,
  );
  expect(duplicate.duplicate).toBe(true);
  expect(duplicate.messageId).toBe(receipt.messageId);
  const sentinel = readFileSync(prepared.sentinel, "utf8");
  f.store.close();
  f = await open(
    prepared.database,
    prepared.state,
    undefined,
    prepared.options,
  );
  expect(
    (await f.service.deny(prepared.fresh.inputId, "deny-retry", note))
      .duplicate,
  ).toBe(true);
  const requests = observe(f);
  await drive(f);
  const model = requests.flat().find((message) => message.role === "tool")!;
  expectRetryFacts(String(model.content), generation, note);
  expect(await f.store.conversations.getMessage(receipt.messageId)).toEqual(
    stored,
  );
  expect(prepared.state.effects).toBe(1);
  expect(readFileSync(prepared.sentinel, "utf8")).toBe(sentinel);
  f.store.close();
});

for (const actualLegacy of [false, true]) {
  it.skipIf(
    actualLegacy &&
      process.env.AGENTKIT_DENIAL_LEGACY_PACKAGE_ROOT === undefined,
  )(
    `${actualLegacy ? "actual .15" : "stored .15-shaped"} denial is enriched only for provider replay`,
    async () => {
      const prepared = await uncertain(
        actualLegacy
          ? process.env.AGENTKIT_DENIAL_LEGACY_PACKAGE_ROOT!
          : undefined,
      );
      let { f } = prepared;
      const note = "Do not repeat the interrupted command.";
      const receipt = await f.service.deny(
        prepared.fresh.inputId,
        "legacy-deny",
        note,
      );
      if (!actualLegacy) {
        const generic = JSON.stringify({
          ok: false,
          status: "error",
          summary: "The user denied this tool call.",
          warnings: [],
          truncated: false,
          data: {
            errorCode: "approval_denied",
            errorMessage: note,
            phase: "guard",
            retryable: false,
          },
        });
        f.store.database.run(
          "UPDATE messages SET content = $envelope, model_result_json = $envelope WHERE id = $id",
          { $envelope: generic, $id: receipt.messageId },
        );
      }
      const stored = (await f.store.conversations.getMessage(
        receipt.messageId,
      ))!;
      expect(
        (JSON.parse(String(stored.content)) as Denial).data.deniedScope,
      ).toBeUndefined();
      const request = await f.store.inputs.getInput(prepared.fresh.inputId);
      const sentinel = readFileSync(prepared.sentinel, "utf8");
      f.store.close();
      f = await open(
        prepared.database,
        prepared.state,
        undefined,
        prepared.options,
      );
      expect(
        (await f.service.deny(prepared.fresh.inputId, "legacy-deny", note))
          .duplicate,
      ).toBe(true);
      const requests = observe(f);
      await drive(f);
      expectRetryFacts(
        String(
          requests.flat().find((message) => message.role === "tool")!.content,
        ),
        (prepared.fresh.content as { approvalGeneration: string })
          .approvalGeneration,
        note,
      );
      expect(await f.store.conversations.getMessage(receipt.messageId)).toEqual(
        stored,
      );
      expect(await f.store.inputs.getInput(prepared.fresh.inputId)).toEqual(
        request,
      );
      expect(prepared.state.effects).toBe(1);
      expect(readFileSync(prepared.sentinel, "utf8")).toBe(sentinel);
      f.store.close();
    },
  );
}

it("unlinked denials and known completed results are never rewritten", async () => {
  const prepared = await uncertain();
  const { f } = prepared;
  const receipt = await f.service.deny(prepared.fresh.inputId, "deny");
  const stored = (await f.store.conversations.getMessage(receipt.messageId))!;
  const generic = JSON.stringify({
    ok: false,
    status: "error",
    data: { errorCode: "approval_denied" },
  });
  for (const patch of [
    { id: "different-answer" },
    { runId: "other-run" },
    { toolCallId: "other-call" },
    { metadata: { ...stored.metadata, requestId: "unrelated" } },
    { metadata: { ...stored.metadata, humanAnswer: false } },
    { metadata: { ...stored.metadata, answerId: "different-answer" } },
  ]) {
    expect(
      await providerHumanAnswerContent(f.store, {
        ...stored,
        ...patch,
        content: generic,
      }),
    ).toBe(generic);
  }
  const completed = JSON.stringify({
    ok: true,
    status: "ok",
    data: { effect: "recorded" },
  });
  expect(
    await providerHumanAnswerContent(f.store, {
      ...stored,
      content: completed,
    }),
  ).toBe(completed);
  const withoutInputs: AssistantStore = {
    ...f.store,
    transaction: f.store.transaction.bind(f.store),
    inputs: undefined,
  };
  await expect(
    providerHumanAnswerContent(withoutInputs, stored),
  ).rejects.toMatchObject({ code: "recovery_history_unreadable" });
  expect(
    await providerHumanAnswerContent(withoutInputs, {
      ...stored,
      content: completed,
    }),
  ).toBe(completed);
  f.store.inputs.getInput = async () => {
    throw new Error("input store unreadable");
  };
  await expect(
    providerHumanAnswerContent(f.store, stored),
  ).rejects.toMatchObject({ code: "recovery_history_unreadable" });
  expect(
    await providerHumanAnswerContent(f.store, {
      ...stored,
      content: completed,
    }),
  ).toBe(completed);
  f.store.close();
});
