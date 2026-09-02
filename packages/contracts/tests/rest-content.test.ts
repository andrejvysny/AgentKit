import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import {
  AiMessageContentSchema,
  MessageDtoSchema,
  SubmitMessageRequestSchema,
} from "../src/index.js";

/**
 * The REST DTOs must carry the SAME content rule as the message contract. They
 * once re-declared the union inline and silently lost `minItems: 1`, so a
 * server generated from the published schemas accepted `content: []` while
 * `docs/contracts.md` promised the schema rejected it.
 */
describe("REST content schemas reuse AiMessageContentSchema", () => {
  const ajv = new Ajv({ strict: false, allErrors: true });
  const asJson = (schema: unknown) => JSON.parse(JSON.stringify(schema));

  it("rejects an empty parts array on submit and on the DTO", () => {
    const submit = ajv.compile(asJson(SubmitMessageRequestSchema));
    const dto = ajv.compile(asJson(MessageDtoSchema));
    const content = ajv.compile(asJson(AiMessageContentSchema));

    expect(content([])).toBe(false);
    expect(submit({ content: [] })).toBe(false);
    expect(
      dto({
        id: "m1",
        chatId: "c1",
        role: "user",
        content: [],
        metadata: {},
        createdAt: "2026-01-01T00:00:00.000Z",
      }),
    ).toBe(false);
  });

  it("still accepts a string and a non-empty parts array", () => {
    const submit = ajv.compile(asJson(SubmitMessageRequestSchema));
    expect(submit({ content: "hi" })).toBe(true);
    expect(submit({ content: [{ type: "text", text: "hi" }] })).toBe(true);
  });
});
