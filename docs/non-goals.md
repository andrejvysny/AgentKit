# Non-goals

Deferred surface — work this repository deliberately does not include yet,
and where each piece is expected to land when it does. None of this is
implemented in `packages/` today; do not import paths named
below.

- **React / UI packages.** No frontend package exists in this monorepo.
  A consuming app (OpenPCB, a cloud-agent service, OneCAD) owns its own UI
  against the `@agentkit/host` port contracts and the `AiRunEvent` stream.
- **OpenPCB compatibility wrapper.** No shim maps `@openpcb/ai-core`'s old
  API onto `@agentkit/core`. OpenPCB's own migration to this package is a
  separate, future change in the OpenPCB repository — this session made
  **no commits** to OpenPCB or task-system, reference-reading only.
- **Cloud-agent Redis/Postgres adapters.** The reference adapters ship only
  an in-memory store (`@agentkit/adapters-memory`) and a `bun:sqlite` store
  (`@agentkit/adapters-sqlite`), both single-process — as is
  `@agentkit/runner-local` (see their READMEs). A
  distributed deployment needs its own `AssistantStore`/`TaskRunner`
  implementation over a networked backend — the port interfaces in
  `packages/host/src/ports/` are what such an adapter implements against.
- **OneCAD `HostBridge`.** No integration code for OneCAD exists here.
  OneCAD would implement the same host ports a desktop or cloud embedding
  does; nothing OneCAD-specific belongs in this repository.
- **Mention pipeline** (tiptap-to-markdown conversion for `@`-mentions in a
  chat composer). This is UI-adjacent behavior that belongs in a consuming
  app's frontend, not in `@agentkit/core` or `@agentkit/host`.
- **Memory port** (long-term/cross-session recall beyond one chat's
  message history). No `MemoryStore` port or implementation exists.
  `ConversationStore` covers one chat's messages; anything spanning chats
  is future scope — see [`docs/roadmap.md`](roadmap.md), P6.
- **Cloud salvage pieces**: token-crypto helpers and trusted-field injection
  for a multi-tenant gateway. Referenced in port documentation
  (`docs/ports.md`, `docs/architecture.md`) as things a durable,
  distributed, multi-tenant deployment will need, but neither is
  implemented. (An SSE-resume client shipped as
  `@agentkit/transport-http`'s `streamRun` — see [ADR
  0005](adr/0005-http-transport.md) — so it is no longer listed here.)
- **npm publishing.** Gated on `@agentkit` npm scope ownership being
  confirmed; see the warning in the root [README](../README.md). Every
  package's `publishConfig` is ready, but nothing is published.
- **AI SDK spike** (evaluating Vercel's `ai` SDK or similar as a provider
  transport). Deferred pending live provider endpoints to test against;
  `@agentkit/core` keeps its hand-rolled `OpenAiCompatibleClient` for now,
  with `stream_options.include_usage` added to close the usage-reporting
  gap that motivated the spike.
- **Multi-pass correction harness.** `VerificationHook` exists and
  `TurnRunner` invokes it exactly once, after a run that made tool calls
  (see [`docs/ports.md`](ports.md#verification)) — but nothing feeds a
  `DeficiencyReport` back into the model for a bounded number of correction
  passes. A single verification invocation is implemented; the harness that
  would loop on it, with its own cost and stopping condition, is future
  work.
- **Durable cross-process cancellation.** The reference `TaskRunner`
  (`SingleProcessTaskRunner`) delivers `requestCancel` by aborting an
  in-memory `AbortController` this process registered for the run; a cancel
  aimed at a run some *other* process is executing does nothing here (its
  `recover()` pass reconciles the run only once that other process's lease
  expires). A durable, cross-process cancel needs a cancellation flag in
  the store that every worker polls — a different design with a different
  cost, not attempted by this reference adapter.
