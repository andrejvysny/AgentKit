import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import type { TSchema } from "@sinclair/typebox";
import {
  CONTRACT_VERSION,
  TaskEventEnvelopeSchema,
  type AiRunEvent,
  type TaskEventEnvelope,
} from "../src/index.js";

const asJson = (schema: TSchema): object =>
  JSON.parse(JSON.stringify(schema)) as object;

describe("TaskEventEnvelopeSchema", () => {
  const validate = new Ajv({ strict: false, allErrors: true }).compile(
    asJson(TaskEventEnvelopeSchema),
  );

  const envelope = {
    type: "index.progress",
    seq: 3,
    eventId: "evt_3",
    timestamp: "2026-01-01T00:00:00.000Z",
    contractVersion: CONTRACT_VERSION,
  };

  it("accepts an event from a task vocabulary it has never heard of", () => {
    expect(validate(envelope)).toBe(true);
    expect(validate({ ...envelope, attemptId: "attempt_2" })).toBe(true);
  });

  it("lets an unknown vocabulary carry its own fields through", () => {
    // The store reads seq and eventId and nothing else; `data`, `runId` and
    // whatever a vocabulary adds must ride through untouched.
    expect(
      validate({ ...envelope, runId: "run_1", data: { done: 7, total: 9 } }),
    ).toBe(true);
  });

  it("requires the two fields the store actually orders and dedups on", () => {
    const { seq: _noSeq, ...withoutSeq } = envelope;
    const { eventId: _noId, ...withoutEventId } = envelope;
    expect(validate(withoutSeq)).toBe(false);
    expect(validate(withoutEventId)).toBe(false);
    expect(validate({ ...envelope, seq: "third" })).toBe(false);
  });

  it("accepts every event of the chat-turn vocabulary", () => {
    const runEvent: AiRunEvent = {
      type: "run.completed",
      runId: "run_1",
      timestamp: "2026-01-01T00:00:00.000Z",
      contractVersion: CONTRACT_VERSION,
      eventId: "evt_9",
      seq: 9,
      data: { iterations: 1, finishReason: "stop" },
    };
    expect(validate(runEvent)).toBe(true);
    // …and at the type level too: AiRunEvent structurally satisfies the
    // envelope, which is what lets one store hold both vocabularies. This
    // assignment is the assertion; the runtime check below just keeps the test
    // honest about having run.
    const asEnvelope: TaskEventEnvelope = runEvent;
    expect(asEnvelope.eventId).toBe("evt_9");
  });
});
