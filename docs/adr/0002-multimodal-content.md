# ADR 0002 — Provider-neutral multimodal message content

**Status:** accepted, implemented (2026-08-24)
**Contract impact:** `CONTRACT_VERSION` 0.1.0 → 0.2.0 (breaking while 0.x).

## Problem

`AiChatMessage.content` was `Type.String()`. Image/file input could not be
expressed at all — the contract, not a missing feature, was the blocker.
OpenPCB (the origin codebase) ships working image mentions by constructing an
OpenAI-style content-part array and casting it through
`as unknown as string`, i.e. even the source project's own contract could not
model what its code sends. Every month of delay bakes `content: string` into
more consumers; at 0.x with zero external consumers, widening is at its
cheapest.

## Decision

1. **`content: string | AiContentPart[]`** (`packages/contracts/src/content.ts`).
   Parts are **provider-neutral**: `{ type: "text", text }` and
   `{ type: "image", source: { kind: "url" | "data", … }, detail? }` — not
   OpenAI's `image_url` blocks, not Anthropic's `source` blocks. Adapters map.
2. **Closed part union.** An unknown part `type` must not validate; new kinds
   (audio, file, …) are additive union members, decided when they arrive.
3. **Mapping lives only in the provider client.**
   `OpenAiCompatibleClient.buildRequestBody` serializes parts natively for
   `user`/`assistant`; for `system`/`tool` it flattens text parts and drops
   image parts, emitting one `run.warning` (`multimodal_flattened`) per
   request — degrade, never fail a request over an attachment.
4. **`messageContentToText()`** (`@agentkit/core`) is the single string-only
   escape hatch for consumers that need text (envelope parsing, probes, log
   rendering).
5. **Host persistence stays string-only this phase.** `MessageRecord.content`
   and the REST `MessageDto` are unchanged, so a multimodal turn cannot yet
   round-trip through `TurnRunner`/`ConversationStore` — the capability lands
   for direct `runChat` embedders. Widening the host record belongs to the
   attachments phase (`docs/roadmap.md`, P2), which owes answers on storage,
   size budgets, and replay that this phase deliberately does not fake.

## Evidence

- OpenPCB `run-service.ts` `buildUserMessageWithImages` (the string-cast
  hack) and its byte/count budgets (`MENTION_LIMITS`) — proof of demand and
  a working reference for the future attachments phase.
- OneMind's `ContentPart {text|image|file|code|reasoning}` model — proof the
  part-union shape serves a real product; its `file`/`code` parts are why the
  union must stay additive.

## Alternatives considered

- **Adopt OpenAI's content-part wire shape directly.** Rejected: the
  framework's domain model would then be one vendor's wire format
  (provider-neutrality is a stated principle), and Anthropic-style adapters
  would translate twice.
- **Separate `attachments` field beside `content`.** Rejected: ordering
  between text and images inside one message is meaningful to models; a
  side-array loses it.
- **Waiting for the attachments phase to widen the contract.** Rejected: the
  contract change is the breaking part and gets more expensive with every
  consumer; the storage work is additive and can follow.

## Consequences

- Assistant *output* remains text: `run.message.completed.data.content` is
  still a string. Revisit only if a target provider emits multimodal
  assistant output.
- Golden traces were re-recorded for the version bump (shape unchanged).
- `@agentkit/testing` fixtures still build string messages; parts are built
  inline where tested.
