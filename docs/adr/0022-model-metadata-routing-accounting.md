# ADR 0022 — Source-attributed model metadata, optional capability-aware routing, deterministic usage accounting

**Status:** accepted, implemented (2026-10-28; Phase B/B2–B4).
**Contract impact:** additive. `AiProviderModel` gained optional capability
facts (`supportsStreaming`/`JsonMode`/`Vision`/`Audio`/`Reasoning`,
`reasoningEfforts`, `maxOutputTokens`) and a versioned `AiModelPricing` block
(`currency` and `priceSource` required on the block; `cachedInputPerMillionTokens`
tracked separately; optional `effectiveDate`), plus `metadataSource`
(`host | provider | preset`) and `metadataFetchedAt`. `run.usage` / `run.started`
gained optional `providerId`. `CONTRACT_VERSION` unchanged; fixtures, mirror and
schema bundle regenerated.

## Problem

The catalogue was thin (id/display name/context window) and unattributed: a
host that *knew* a model's real context window or prices had nowhere to say so
that survived a provider refresh, and consumers could not tell an advertised
fact from a guess. With thin metadata, any routing decision is fabrication.
Routing itself risked the core invariant — "one unified execution engine, one
durable orchestration owner" — if a second loop appeared to hold model choice.

## Decision

Three layers, each optional, each refusing to fabricate.

1. **Metadata with provenance** (contracts + `mergeModelMetadata` in core).
   Field-level precedence `host > provider > preset`; identity fields belong to
   the record being described, never overwritten by a lower source; absent
   means *unknown*, never defaulted. `refreshProviderModels` merges explicit
   host-declared metadata over the provider snapshot before `replaceModels`, so
   truth survives refresh; vLLM context-window detection keeps working, and
   server-advertised entries are tagged `metadataSource: "provider"`.
2. **Routing as a loop seam, not a loop** (`runs/model-router.ts`).
   `runAgentLoop` accepts an optional `modelRouter`; absent, the run is pinned
   to `client` + `model` — the exact pre-B3 path (measured: no regression,
   `docs/baseline/phase-b-2026-10-28.md`). Present, it is consulted before
   every provider request; the loop still owns iterations, budget and events.
   Invariants enforced by the runtime, not the router: selections must be
   well-formed; the host must authorize every selection before dispatch;
   cross-provider switches require `resolveClient` (no silent remote fallback);
   opaque provider continuation state is never forwarded across a model switch
   — the run fails `continuation_conflict` unless the host explicitly chose
   `continuationModelSwitch: "reset"`; routed clients go through the same
   budget wrapper (`wrapClient` seam) as the run's own client, so routed
   requests consume the same `ExecutionBudget`.
   `createCapabilityAwareRouter` selects only models whose declared metadata
   *proves* every required capability (absent facts never count as satisfied;
   no match fails closed with `no_capable_model`); the previous model is sticky
   while it qualifies; a `prefer` hook allows cost/latency ranking without
   making speculative cost routing the default.
3. **Accounting folded from events** (`runs/usage-accounting.ts`,
   `runs/observability.ts`). `summarizeRunUsage` derives everything —
   per provider/model totals, billed calls deduped by `callId` (final wins over
   stream drafts; cut-short calls bill once), retries from `provider_retry`
   warnings, tool/approval-wait/TTFT durations from event timestamps — from the
   event stream alone, so replay recomputes identical accounting.
   `estimateRunCost` prices only from B2 metadata: cached input billed
   separately (fallback to input rate is flagged), an unknown price is *unknown*
   — never zero — with status `priced | partial | unknown` and totals per
   currency. Telemetry is an OTel-shaped opt-in sink emitting identity,
   counts and durations only — never prompts, user content, credentials or
   hidden reasoning — and every emit is firewalled, so observability cannot
   fail a run.

## Consequences

- Hosts can state model truth and have it survive refresh; unknowns stay
  unknown in metadata, routing and cost alike.
- Multi-provider runs remain one loop with one budget: routing refusals
  terminate the run with a normalized `run.failed` whose `errorCode` is the
  routing code (`invalid_selection`, `provider_unresolved`,
  `continuation_conflict`, `authorization_denied`, `no_capable_model`).
- Usage/cost/latency answers are reproducible from persisted events — the
  same evidence base the cassette replays against (ADR 0021).
