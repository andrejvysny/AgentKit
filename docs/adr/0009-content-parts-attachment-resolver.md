# ADR 0009 — Content parts at persistence, and `AttachmentResolver`

**Status:** accepted, implemented (2026-09-01; resolver context widened
2026-09-02)
**Contract impact:** `CONTRACT_VERSION` `0.3.0` → `0.4.0`. Classified
breaking despite the type-level change being a widening (`string` →
`string | AiContentPart[]`, still accepting a plain string everywhere it
always did): any existing code that assumed `content` was *always* a string —
every call site in this repository included — now has to handle the array
case, which is exactly the class of change the `CONTRACT_VERSION` policy
bumps the minor for while the contract is `0.x` (see
[`docs/contracts.md`](../contracts.md)).

## Problem

`@agentkit/core`'s provider-neutral content-part model
([ADR 0002](0002-multimodal-content.md)) mapped parts onto the wire shape a
provider expects, but stopped at that boundary: `MessageRecord.content`,
`SubmitMessageInput.content`, and the REST `MessageDto.content` /
`SubmitMessageRequest.content` were all plain `string`. Neither of the two
target consumers this framework is being extracted for could migrate onto
it as-is: OpenPCB's chat composer sends image mentions (screenshots dropped
into a message), and OneMind's chat needs vision input — both need to
**persist** more than text, not just send it to a provider once.

## Evidence

- The originating plan's consumer inventory names this as a hard blocker,
  file:line: `packages/host/src/ports/conversation-store.ts` (the record
  shape), `packages/host/src/turn/turn-runner.ts` (submit), and
  `packages/contracts/src/rest.ts` (the wire DTO) were all `string`-only.
- OpenPCB's own `MENTION_LIMITS` as the reference budget numbers (5 MiB per
  image, 20 MiB aggregate) — the same reference [`docs/roadmap.md`](../roadmap.md)'s
  P5c entry had already flagged before this landed.
- OneMind's own multi-part search bug — indexing only the **first** text
  part of a multi-part message — named here as a defect to avoid porting,
  the same posture [ADR 0004](0004-mcp-client.md) took toward OneMind's MCP
  transport defects.
- A Phase B/C fresh-context verifier pass found a second hazard one day after
  landing: `AttachmentResolver.resolve(ref)` took no context at all, so a
  resolver had no way to answer "may **this chat** see these bytes" as
  opposed to "do these bytes exist anywhere." A `ref` is untrusted client
  input — whatever string a caller put in a message part; AgentKit neither
  mints nor parses one — so a resolver that looked one up globally would hand
  one chat's (or one tenant's) attachments to anyone who could guess a ref
  naming them.

## Decision

1. **`AiContentPart` at persistence, not just at the provider boundary.**
   `MessageRecord.content`, `AppendMessageInput.content`,
   `UpdateMessagePatch.content`, `SubmitMessageInput.content`, and the REST
   `MessageDto.content` / `SubmitMessageRequest.content` all become
   `string | AiContentPart[]` (`AiMessageContent`). A store round-trips parts
   **losslessly** and inspects nothing inside them.
2. **A third image source: `{ kind: "ref", ref }`**, beside the existing
   wire-value sources `url` and `data`. A `ref` is an opaque handle into the
   **host's** own blob storage — AgentKit never parses one, derives a path
   from one, or mints one. Storing the handle instead of inlined bytes is
   what keeps a conversation with a 4 MB screenshot in it cheap to append to,
   to fork, and to page through, and what lets the same stored message be
   re-resolved — or refused — at a different fidelity on a later turn.
3. **New port: `AttachmentResolver.resolve(ref, ctx) → { mediaType, base64 }
   | null`.** Resolution happens in `TurnRunner`, per pass, after
   `assembleMessages` and before `runChat`, for every pass including
   retries — cached **within** a pass, never across passes, because the world
   moves between turns (an attachment can be deleted, expire, or belong to a
   workspace access to which was revoked). `null` is a **normal** answer
   (drop the part, emit a durable warning), never an error path; throw only
   for a genuine fault (storage down), where failing the turn is honest.
4. **Budgets, borrowed verbatim from OpenPCB's `MENTION_LIMITS`**: 5 MiB per
   image, 20 MiB aggregate, 16 images per pass — configurable via
   `TurnRunnerDeps.attachmentBudgets`. An image that cannot be sent
   (unresolvable, or over a cap) is dropped from what the provider sees, with
   one durable `run.warning` naming the ref and the reason
   (`attachment_unresolved`, `attachment_budget_exceeded`) — degrade, never
   fail a turn, over an attachment. The stored message always keeps the ref
   regardless of what any pass resolved.
5. **Search indexes all text parts, not just the first.** `searchTextOf`
   joins every text part of a message with `"\n"` before anything is
   indexed — closing, rather than porting, the exact bug named in Evidence
   from OneMind's own search.
6. **sqlite `SCHEMA_V5`** adds a `content_format` column beside `content`
   (`text | parts`) so the adapter round-trips either representation without
   re-parsing to guess which one it is looking at; the memory adapter
   **deep-copies** parts arrays on every read and write — the round-trip
   losslessness invariant applied to a Map-backed store, where "lossless"
   also means "a caller cannot mutate the stored value through a reference it
   was handed back."
7. **`resolve`'s signature widened the next day, same wave: `resolve(ref, {
   chatId })`.** Reframes `AttachmentResolver.resolve` as an
   **authorization** question, not a lookup — a ref is untrusted client
   input, so the real question a resolver answers is "may **this chat** see
   these bytes," not "do these bytes exist," and `null` is also the honest
   answer for a ref belonging to another chat, another tenant, or a workspace
   the caller lost access to. A resolver that ignores the passed context and
   looks a ref up globally hands one chat's attachments to anyone who can
   guess a ref that names them.

## Alternatives considered

- **Inline resolved bytes into the stored record**, so a resolver only ever
  runs once per attachment. Rejected: makes every fork and every page of the
  conversation carry the attachment's bytes forever, and forecloses
  re-resolving (or re-refusing) an attachment differently as access changes
  on a later turn.
- **A single aggregate byte budget, no per-image cap.** Rejected: OpenPCB's
  own `MENTION_LIMITS` already distinguish "one huge image" from "many small
  ones" as different failure modes, and a single aggregate cap conflates
  them into one number that cannot express both.
- **Leave `resolve(ref)` context-free and push tenant scoping into the ref
  string itself** (e.g. embedding a `chatId` inside the ref). Rejected:
  a ref is caller-supplied content a client can construct freely; encoding
  trust into the STRING rather than checking it against the caller's actual
  chat at resolve time is exactly the kind of implicit trust a multi-tenant
  resolver cannot safely rely on.

## Consequences

- A host that wires no `AttachmentResolver` still runs correctly — a
  conversation carrying refs drops each ref-sourced image with a warning; a
  host that never writes refs never notices the port exists.
- `AttachmentResolver.resolve`'s second parameter is required for any new
  adapter from this point on — the widening landed before any external
  consumer existed, at the cheapest possible moment to change a port
  signature.
- `CONTRACT_VERSION` moves `0.3.0` → `0.4.0`; golden traces were re-recorded
  once, in the same commit as the persistence change.
- [`docs/roadmap.md`](../roadmap.md)'s P5c entry (attachments) moves to Done,
  with attachment **blob storage** noted explicitly as staying a host
  concern — this ADR ships the parts model, the `ref` indirection, and the
  resolution/budget machinery, not a bundled file store.

## Out of scope (deliberate)

A bundled blob/file store (storage stays the host's own concern — a file, a
row, an S3 key, a content-addressed cache); non-image content parts (audio,
file — the union is closed and additive, decided when a real need arrives,
per [`docs/contracts.md`](../contracts.md)); resolver-side caching across
passes or runs (per-pass only, by design, since the world moves between
turns).
