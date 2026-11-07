/**
 * Per-turn `reasoningEffort` on the two turn routes.
 *
 * Both hosts already honour the field — the Rust host reads it off the request
 * body in `crates/host/src/http/turns.rs`, and the TypeScript host takes it on
 * `SubmitMessageInput` / `RegenerateMessageInput` — so what is pinned here is
 * the REST layer in between, which is where a level can quietly disappear:
 *
 * - A level the caller sent must reach the DURABLE task payload. A route that
 *   dropped it would answer the question with a different amount of thinking
 *   than the caller asked for, and nothing in the transcript would say so.
 * - A level the contract does not admit must be refused, not rounded to the
 *   nearest neighbour, and a refusal must leave no message or branch behind.
 * - Omitting the field must stay distinct from sending the lowest level: the
 *   session's own effort is what stands when nothing was said.
 * - A replayed `Idempotency-Key` must not be allowed to change it, which is why
 *   the level is part of the replay fingerprint on both routes.
 */
import { describe, expect, it } from "bun:test";
import type { SubmitMessageResponse } from "@agentkit/contracts";
import {
  createHandlerFixture,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

async function setup() {
  const environment = await createHandlerFixture();
  const submit = (key: string, extra: Record<string, unknown> = {}) =>
    environment.handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content: "why is the sky blue?", ...extra },
        headers: { "idempotency-key": key },
      }),
    );
  const seeded = await (await submit("seed")).json();
  const answerId = (seeded as SubmitMessageResponse).assistantMessageId;
  const regenerate = (key: string, extra: Record<string, unknown> = {}) =>
    environment.handler(
      request(
        "POST",
        `/v1/chats/${TEST_CHAT_ID}/messages/${answerId}/regenerate`,
        { body: { ...extra }, headers: { "idempotency-key": key } },
      ),
    );
  const payloadOf = async (taskId: string): Promise<Record<string, unknown>> =>
    (await environment.store.tasks.getTask(taskId))?.payload as Record<
      string,
      unknown
    >;
  return { ...environment, submit, regenerate, payloadOf };
}

async function expectProblem(
  response: Response,
  status: number,
  code: string,
): Promise<void> {
  expect(response.status).toBe(status);
  // Narrowed, not asserted: `code` is read off whatever the handler produced,
  // and a body without one must fail the expectation below rather than read as
  // an assumed `undefined`.
  const body: unknown = await response.json();
  const actual =
    body && typeof body === "object" && "code" in body ? body.code : undefined;
  expect(actual).toBe(code);
}

describe("per-turn reasoning effort", () => {
  it("carries the level the caller sent into the durable task payload", async () => {
    const f = await setup();
    const accepted = await f.submit("high", { reasoningEffort: "high" });
    expect(accepted.status).toBe(201);
    const { runId } = (await accepted.json()) as SubmitMessageResponse;
    expect((await f.payloadOf(runId))["reasoningEffort"]).toBe("high");
  });

  it("leaves the field absent when the caller sent nothing, rather than defaulting it", async () => {
    const f = await setup();
    const accepted = await f.submit("plain");
    expect(accepted.status).toBe(201);
    const { runId } = (await accepted.json()) as SubmitMessageResponse;
    expect("reasoningEffort" in (await f.payloadOf(runId))).toBe(false);
  });

  it("refuses a level the contract does not admit and leaves nothing behind", async () => {
    const f = await setup();
    const before = await f.store.conversations.listMessages(TEST_CHAT_ID);
    for (const level of ["ultra", "MINIMAL", 1, true, [], null, {}]) {
      await expectProblem(
        await f.submit(`bad-${JSON.stringify(level)}`, {
          reasoningEffort: level,
        }),
        400,
        "invalid_request",
      );
    }
    expect(await f.store.conversations.listMessages(TEST_CHAT_ID)).toEqual(
      before,
    );
  });

  it("will not let a replayed key change the level of the turn it already admitted", async () => {
    const f = await setup();
    const first = await (
      await f.submit("same", { reasoningEffort: "high" })
    ).json();
    expect(
      (await f.payloadOf((first as SubmitMessageResponse).runId))[
        "reasoningEffort"
      ],
    ).toBe("high");
    await expectProblem(
      await f.submit("same", { reasoningEffort: "low" }),
      422,
      "idempotency_key_mismatch",
    );
    const replay = await f.submit("same", { reasoningEffort: "high" });
    expect(replay.status).toBe(200);
    expect(await replay.json()).toEqual(first);
  });

  it("carries the level on a regenerate, and refuses to change it on replay", async () => {
    const f = await setup();
    const accepted = await f.regenerate("again", { reasoningEffort: "low" });
    expect(accepted.status).toBe(201);
    const first = (await accepted.json()) as SubmitMessageResponse;
    expect((await f.payloadOf(first.runId))["reasoningEffort"]).toBe("low");
    await expectProblem(
      await f.regenerate("again", { reasoningEffort: "high" }),
      422,
      "idempotency_key_mismatch",
    );
    const replay = await f.regenerate("again", { reasoningEffort: "low" });
    expect(replay.status).toBe(200);
    expect(await replay.json()).toEqual(first);
  });

  it("refuses an unadmitted level on a regenerate too", async () => {
    const f = await setup();
    await expectProblem(
      await f.regenerate("bad", { reasoningEffort: "sometimes" }),
      400,
      "invalid_request",
    );
  });
});
