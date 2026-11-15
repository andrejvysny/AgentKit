# ADR 0028: Cache-stable prompt layout

Status: accepted (2026-11-15). Implements CE-01 of the context efficiency
spec (roadmap package 1.1). Builds on ADR 0017 (project instructions) and
ADR 0022 (Rust owns the wire contract).

## Context

Provider prompt caches match on the byte prefix of a request. AgentKit built
the head of the prompt from one joined system string (profile text, context
text, project rules, delegation guidance) with selected skills ahead of it.
Any change to one part rewrote every byte after it, and nothing recorded what
the head had been. The prefix-stability harness (package 0.3) measured the
resulting breaks.

## Decision

1. **Six segments, fixed order.** A request is S1 tools, S2 system, S3 project
   rules, S4 core memory, S5 history, S6 the new turn. S1-S4 are the frozen
   head. `agentkit_core::Segment` names them; `PromptLayout` holds the system
   segments S2-S4 as ordered blocks. Empty segments are omitted.
2. **System blocks stay separate to the wire.** The chat executor builds one
   `system` message per block, tagged `metadata["agentkit.segment"]`.
   - Chat Completions sends each as its own `system` message, in order.
   - Responses carries S2 in `instructions` (the lane's single string) and
     S3/S4 as leading system-role `input` items. They are request-local like
     selected content and are never checkpointed into continuation state.
   - Block-structured transports (Anthropic) read the tagged messages, or
     `PromptLayout::text_blocks`, so S2-S4 become three text blocks and a cache
     breakpoint sits on a real block boundary. The Anthropic request builder
     lives in OneMind and consumes this after the next vendor sync.
   - Untagged system messages keep today's behaviour.
3. **Delegation guidance moves into S2.** It was appended after the project
   rules; S3 is now its own block, so S2 holds profile, resolved context and
   delegation text.
4. **`SessionPrefix` is a recorded contract.** `AiSessionPrefix` (Rust DTO,
   schema bundle, TypeBox) holds a revision, SHA-256 digests of S1-S4 and the
   frozen request parameters, the freezing task, a cache key
   (`base32(sha256(chat, branch, revision))[..32]`) and three optional
   tool-track fields: `toolManifestHash` (1.3), `modelProfileId` and
   `dialectVersion` (native tool use T2.1). It is stored in the session's chat
   metadata under `agentkit.session.prefix`.
5. **Freeze rule.** The first session turn records revision 1. Each later turn
   recomputes the digests. Equal digests keep the record. Different digests
   replace it under revision + 1 and emit a durable
   `run.warning { code: "prefix_refrozen", revision, changed }`, because that
   turn rewrites bytes a cache may hold (INV-3).

## Consequences

- Rewrites of the head are visible per session instead of silent. Edited
  project rules still re-freeze immediately; keeping the old S3 and sending a
  delta note is package 1.2 (turn context), and the sorted tool manifest is 1.3.
  Until then the harness still reports those breaks.
- `context.cache_break` is not yet a contract event. `prefix_refrozen` is the
  durable marker until the event type is added; the prefix-stability harness
  counts both as the recorded exception INV-3 allows.
- Selected skills and templates still precede S2 on Chat Completions and in
  Responses input; package 1.2 moves them into the user message.
- The correction pass still receives S2-S4 joined as one string.
- Enforcement stays in report mode until package 1.8.
