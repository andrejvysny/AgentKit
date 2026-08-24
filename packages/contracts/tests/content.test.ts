import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import type { TSchema } from "@sinclair/typebox";
import {
  AiChatMessageSchema,
  AiContentPartSchema,
  AiImageSourceSchema,
  AiMessageContentSchema,
  type AiChatMessage,
  type AiContentPart,
} from "../src/index.js";

const asJson = (schema: TSchema): object =>
  JSON.parse(JSON.stringify(schema)) as object;

const makeAjv = () => new Ajv({ strict: false, allErrors: true });

describe("AiMessageContentSchema", () => {
  const validate = makeAjv().compile(asJson(AiMessageContentSchema));

  it("accepts a plain string — the shape every pre-multimodal caller sends", () => {
    expect(validate("hello")).toBe(true);
    expect(validate("")).toBe(true);
  });

  it("accepts a parts array mixing text, a url image and an inline data image", () => {
    expect(
      validate([
        { type: "text", text: "what is on this board?" },
        {
          type: "image",
          source: { kind: "url", url: "https://example.test/board.png" },
          detail: "high",
        },
        {
          type: "image",
          source: {
            kind: "data",
            base64: "aGVsbG8=",
            mediaType: "image/png",
          },
        },
      ]),
    ).toBe(true);
    // An empty parts array is structurally fine; whether it is useful is the
    // caller's problem, not the schema's.
    expect(validate([])).toBe(true);
  });

  it("rejects anything that is neither a string nor a parts array", () => {
    expect(validate(42)).toBe(false);
    expect(validate(null)).toBe(false);
    expect(validate({ type: "text", text: "not wrapped in an array" })).toBe(
      false,
    );
  });
});

describe("AiContentPartSchema", () => {
  const validate = makeAjv().compile(asJson(AiContentPartSchema));

  it("rejects an out-of-vocabulary part type", () => {
    // The union is closed on purpose: accepting an unknown part would let a
    // consumer forward content it cannot render.
    expect(validate({ type: "audio", url: "https://example.test/a.mp3" })).toBe(
      false,
    );
    expect(validate({ type: "text" })).toBe(false);
    expect(validate({ type: "text", text: 7 })).toBe(false);
  });

  it("rejects an image part whose source is malformed", () => {
    const source = makeAjv().compile(asJson(AiImageSourceSchema));
    // A data source without its media type is unmappable — an adapter cannot
    // build a `data:` URL out of it.
    expect(source({ kind: "data", base64: "aGVsbG8=" })).toBe(false);
    expect(source({ kind: "url" })).toBe(false);
    expect(source({ kind: "file", path: "/tmp/x.png" })).toBe(false);
    expect(
      validate({ type: "image", source: "https://example.test/board.png" }),
    ).toBe(false);
    expect(
      validate({
        type: "image",
        source: { kind: "url", url: "https://example.test/board.png" },
        detail: "ultra",
      }),
    ).toBe(false);
  });
});

describe("AiChatMessageSchema with multimodal content", () => {
  const validate = makeAjv().compile(asJson(AiChatMessageSchema));

  it("accepts both a string body and a parts body on a user message", () => {
    expect(validate({ role: "user", content: "hi" })).toBe(true);
    expect(
      validate({
        role: "user",
        content: [{ type: "text", text: "hi" }],
      }),
    ).toBe(true);
  });

  it("carries the widened content through to the TS type", () => {
    const parts: AiContentPart[] = [
      { type: "text", text: "look" },
      {
        type: "image",
        source: { kind: "url", url: "https://example.test/x.png" },
      },
    ];
    const message: AiChatMessage = { role: "user", content: parts };
    const stringMessage: AiChatMessage = { role: "system", content: "be terse" };
    expect(Array.isArray(message.content)).toBe(true);
    expect(stringMessage.content).toBe("be terse");
  });
});
