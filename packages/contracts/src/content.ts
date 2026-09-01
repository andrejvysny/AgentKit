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

/**
 * A bare `type/subtype` over RFC 6838's restricted-name characters, matched
 * case-insensitively — no parameters, no whitespace, nothing that needs
 * quoting.
 *
 * Not pedantry about IANA: an adapter builds a
 * `` `data:${mediaType};base64,${base64}` `` URL out of this string (see
 * `toOpenAiContentPart` in `@agentkit/core`), so a `;` or a `,` smuggled in
 * here would end the media-type field early and hand the provider a URL that
 * decodes to something the caller never sent.
 */
const MEDIA_TYPE_PATTERN =
  "^[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*/[A-Za-z0-9][A-Za-z0-9!#$&^_.+-]*$";

export const AiTextPartSchema = Type.Object({
  type: Type.Literal("text"),
  text: Type.String(),
});
export type AiTextPart = Static<typeof AiTextPartSchema>;

/**
 * Where the bytes of an image part come from. `url` points at something the
 * provider fetches; `data` inlines base64 the caller already holds, so a host
 * can attach a screenshot it never published anywhere; `ref` names bytes the
 * HOST holds and has not inlined.
 *
 * **`ref` is the only source a provider never sees.** `url` and `data` are wire
 * values — an adapter serializes them directly. A `ref` is an opaque handle into
 * the host's own blob storage, and it is the host that turns it into a `data`
 * source (per provider pass, in memory, under byte budgets) before a request is
 * built; see `AttachmentResolver` in `@agentkit/host`. Storing the handle rather
 * than the bytes is what keeps a conversation replayable without carrying
 * megabytes of base64 through every append, every fork and every `listMessages`
 * page — and what lets the same stored message be re-resolved at a different
 * fidelity, or refused, on a later turn.
 *
 * A `ref` that reaches a provider client is therefore a host bug, not a wire
 * shape: the client drops the part rather than inventing a URL for it.
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
    /** Constrained by {@link MEDIA_TYPE_PATTERN}. */
    mediaType: Type.String({
      description: 'IANA media type of the payload, e.g. "image/png".',
      pattern: MEDIA_TYPE_PATTERN,
    }),
  }),
  Type.Object({
    kind: Type.Literal("ref"),
    /**
     * Opaque to everything but the host that minted it. Deliberately untyped
     * beyond "a string": a content hash, a row id, a path — the contract has no
     * business constraining a namespace only one side can interpret.
     */
    ref: Type.String({
      description:
        "Host-resolved attachment handle. Never sent to a provider as-is.",
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
  // `minItems: 1`: an empty parts array is not "a message with no content", it
  // is a caller bug — OpenAI rejects `content: []` outright, so the only thing
  // permitting it buys is discovering the mistake as a provider error instead
  // of a validation error. A genuinely empty body is the empty STRING.
  Type.Array(AiContentPartSchema, { minItems: 1 }),
]);
export type AiMessageContent = Static<typeof AiMessageContentSchema>;
