import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import type { TSchema } from "@sinclair/typebox";
import {
  AiChatMessageSchema,
  AiContentPartSchema,
  AiImageSourceSchema,
  AiMessageContentSchema,
  IMAGE_URL_PATTERN,
  MEDIA_TYPE_PATTERN,
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
  });

  it("rejects an empty parts array", () => {
    // Not "a message with no content" — a caller bug. OpenAI rejects
    // `content: []`, so accepting it here only moves the diagnosis from a
    // validation error to a provider error. The empty body is the empty string.
    expect(validate([])).toBe(false);
    expect(validate("")).toBe(true);
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

  it("rejects a mediaType that could break out of a data: URL", () => {
    const source = makeAjv().compile(asJson(AiImageSourceSchema));
    const withType = (mediaType: string): unknown => ({
      kind: "data",
      base64: "aGVsbG8=",
      mediaType,
    });
    // `toOpenAiContentPart` interpolates this straight into
    // `data:<mediaType>;base64,<payload>`; a `;` or `,` would end the field
    // early and change what the provider decodes.
    expect(source(withType("image/png;base64,AAAA"))).toBe(false);
    expect(source(withType("image/png,x"))).toBe(false);
    expect(source(withType("image/png; charset=utf-8"))).toBe(false);
    expect(source(withType("image png"))).toBe(false);
    expect(source(withType("image/"))).toBe(false);
    expect(source(withType("/png"))).toBe(false);
    expect(source(withType("imagepng"))).toBe(false);
    // …while the shapes a real caller sends still pass, in either case.
    expect(source(withType("image/png"))).toBe(true);
    expect(source(withType("image/svg+xml"))).toBe(true);
    expect(source(withType("IMAGE/PNG"))).toBe(true);
    expect(source(withType("application/vnd.foo.bar-1"))).toBe(true);
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

  it("rejects a url source whose scheme is not http(s)", () => {
    const source = makeAjv().compile(asJson(AiImageSourceSchema));
    // The whole point of the pattern: a `url` part reaches the provider
    // verbatim, and a provider client runs in the HOST's network position.
    expect(source({ kind: "url", url: "file:///etc/passwd" })).toBe(false);
    expect(source({ kind: "url", url: "javascript:alert(1)" })).toBe(false);
    expect(source({ kind: "url", url: "data:image/png;base64,aGk=" })).toBe(
      false,
    );
    expect(source({ kind: "url", url: "//example.test/x.png" })).toBe(false);
    expect(source({ kind: "url", url: "https://example.test/x.png" })).toBe(
      true,
    );
    // Plain http stays legal: a local model server on a LAN address is the
    // ordinary desktop deployment, not an attack.
    expect(source({ kind: "url", url: "http://127.0.0.1:1234/x.png" })).toBe(
      true,
    );
  });

  it("publishes the two source patterns other packages must restate", () => {
    // Both are exported so a surface that validates AHEAD of the schema (the
    // REST adapter's hand-written body checks) enforces the same rule rather
    // than a copy of it that drifts.
    expect(
      new RegExp(IMAGE_URL_PATTERN).test("https://example.test/x.png"),
    ).toBe(true);
    expect(new RegExp(IMAGE_URL_PATTERN).test("file:///etc/passwd")).toBe(
      false,
    );
    expect(new RegExp(MEDIA_TYPE_PATTERN).test("image/png")).toBe(true);
    expect(new RegExp(MEDIA_TYPE_PATTERN).test("image/png;x=1")).toBe(false);
  });

  it("accepts a host attachment ref, and rejects one that carries no ref", () => {
    const source = makeAjv().compile(asJson(AiImageSourceSchema));
    expect(source({ kind: "ref", ref: "blob:sha256-abc123" })).toBe(true);
    // Opaque by design: any string is a legal handle, because only the host
    // that minted it can interpret one.
    expect(source({ kind: "ref", ref: "42" })).toBe(true);
    expect(source({ kind: "ref" })).toBe(false);
    expect(source({ kind: "ref", ref: 42 })).toBe(false);
    // A ref is a source, not a shortcut past the part shape.
    expect(
      validate({ type: "image", source: { kind: "ref", ref: "blob:x" } }),
    ).toBe(true);
    expect(validate({ type: "image", ref: "blob:x" })).toBe(false);
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
    const stringMessage: AiChatMessage = {
      role: "system",
      content: "be terse",
    };
    expect(Array.isArray(message.content)).toBe(true);
    expect(stringMessage.content).toBe("be terse");
  });
});
