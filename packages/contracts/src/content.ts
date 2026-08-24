/**
 * Multimodal message content: the provider-neutral shape a message body takes
 * when it is more than a string.
 *
 * **Provider-neutral by design.** These are NOT OpenAI's `image_url` blocks,
 * nor Anthropic's `source` blocks — an adapter maps these parts onto whatever
 * its provider speaks (`@agentkit/core`'s `OpenAiCompatibleClient` does exactly
 * that). A host assembling a message never has to know which provider will
 * eventually serve the run.
 *
 * **Where parts are legal.** A parts array is meaningful on `user` and
 * `assistant` messages. `system` and `tool` messages keep string semantics:
 * every provider models a system prompt as plain text, and a tool result is a
 * serialized envelope by construction. A client handed parts on one of those
 * roles flattens them to text and drops the non-text parts, warning as it goes
 * (the `multimodal_flattened` code in `./run-events.ts`) rather than throwing —
 * a dropped image is a degraded turn, not a broken one.
 */
import { Type, type Static } from "@sinclair/typebox";

export const AiTextPartSchema = Type.Object({
  type: Type.Literal("text"),
  text: Type.String(),
});
export type AiTextPart = Static<typeof AiTextPartSchema>;

/**
 * Where the bytes of an image part come from. `url` points at something the
 * provider fetches; `data` inlines base64 the caller already holds, so a host
 * can attach a screenshot it never published anywhere.
 */
export const AiImageSourceSchema = Type.Union([
  Type.Object({
    kind: Type.Literal("url"),
    url: Type.String(),
  }),
  Type.Object({
    kind: Type.Literal("data"),
    base64: Type.String({
      description: "Raw base64 payload — no `data:` prefix, no media type.",
    }),
    mediaType: Type.String({
      description: 'IANA media type of the payload, e.g. "image/png".',
    }),
  }),
]);
export type AiImageSource = Static<typeof AiImageSourceSchema>;

export const AiImagePartSchema = Type.Object({
  type: Type.Literal("image"),
  source: AiImageSourceSchema,
  /**
   * Requested fidelity, when the provider supports one. Advisory: a provider
   * with no such control ignores it, and no adapter may fail a request over it.
   */
  detail: Type.Optional(
    Type.Union([
      Type.Literal("auto"),
      Type.Literal("low"),
      Type.Literal("high"),
    ]),
  ),
});
export type AiImagePart = Static<typeof AiImagePartSchema>;

/**
 * The part union is deliberately **closed**: an unknown `type` must not
 * validate, because a consumer that silently accepted one would forward content
 * it cannot render. New part kinds (audio, file, …) are additive changes to this
 * union, not an escape hatch left open in advance.
 */
export const AiContentPartSchema = Type.Union([
  AiTextPartSchema,
  AiImagePartSchema,
]);
export type AiContentPart = Static<typeof AiContentPartSchema>;

/**
 * A message body: a plain string (the overwhelmingly common case, unchanged
 * from before multimodal existed) or an ordered array of parts. Widening rather
 * than replacing is what keeps every existing string caller valid.
 */
export const AiMessageContentSchema = Type.Union([
  Type.String(),
  Type.Array(AiContentPartSchema),
]);
export type AiMessageContent = Static<typeof AiMessageContentSchema>;
