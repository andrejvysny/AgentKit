import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import { SubmitMessageRequestSchema } from "../src/index.js";

describe("root submit contract", () => {
  const validate = new Ajv({ strict: true }).compile(
    JSON.parse(JSON.stringify(SubmitMessageRequestSchema)),
  );
  it("accepts explicit null, a message id, and an omitted parent", () => {
    expect(validate({ content: "Root", parentMessageId: null })).toBe(true);
    expect(validate({ content: "Branch", parentMessageId: "parent" })).toBe(
      true,
    );
    expect(validate({ content: "Append" })).toBe(true);
  });
  it("rejects other parent types", () => {
    for (const value of [true, 1, {}, []])
      expect(validate({ content: "Question", parentMessageId: value })).toBe(
        false,
      );
  });
});
