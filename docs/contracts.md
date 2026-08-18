# Contracts

`@agentkit/contracts` is the serialized surface every other package and
every embedding host agrees on. This document covers the run-event
vocabulary, the tool envelope, usage accounting, the generic-kind pattern,
and the TypeBox conventions the package is built on.

Source: [`packages/contracts/src/`](../packages/contracts/src/).

## The event vocabulary

Twelve event types, all discriminated on `type`
([`packages/contracts/src/run-events.ts`](../packages/contracts/src/run-events.ts)):

| Type | Payload (`data`) summary |
|---|---|
| `run.started` | `{ model, toolCount }` — the run began; which model, how many tools were advertised. |
| `run.message.delta` | `{ delta }` — one streamed content chunk. Display-only; not authoritative. |
| `run.message.completed` | `{ content, toolCallCount, toolCalls?, reasoningContent?, finishReason? }` — the authoritative turn content and tool calls, for streaming and non-streaming providers alike. |
| `run.tool.requested` | `{ toolCallId, toolName, argumentsJson }` — a tool call was announced, before execution. |
| `run.tool.running` | `{ toolCallId, toolName }` — execution started. |
| `run.tool.succeeded` | `{ toolCallId, toolName, resultJson, sources, truncated, warnings, status?, summary?, modelResultJson? }` — the tool ran and reported success (or `partial`). `resultJson` is the full data; `modelResultJson` is the slim envelope fed to the model. |
| `run.tool.failed` | `{ toolCallId, toolName, errorMessage, errorCode?, modelResultJson?, status? }` — the call failed, was skipped (cap), or was cancelled. `modelResultJson` is present only when the tool actually ran and reported failure. |
| `run.warning` | `{ code, message }` — a non-fatal condition; see the warning-code table below. |
| `run.usage` | `{ callId, attempt, step, model, promptTokens?, completionTokens?, totalTokens?, source, finalForCall }` — token accounting for one provider call. See "usage" below. |
| `run.completed` | `{ iterations, finishReason? }` — the run ended with an answer. |
| `run.failed` | `{ errorMessage, errorCode? }` — the run ended in a provider or transport error. |
| `run.cancelled` | `{ reason? }` — the run was aborted. |

## The v2 base fields

Every event carries these fields alongside `type` and `data`
(`AiRunEventBaseSchema`):

| Field | Semantics |
|---|---|
| `runId` | The run these events belong to. |
| `timestamp` | ISO-8601 emission time. **Not an ordering key** — two events can share a millisecond, and clocks move. |
| `contractVersion` | The `CONTRACT_VERSION` the emitter spoke, so a consumer reading a persisted stream knows which shape it is looking at instead of guessing from the fields present. |
| `eventId` | Unique per event. The key deduplication and acknowledgement use when a stream is replayed or redelivered. |
| `seq` | Strictly increasing within a run. The real ordering key — lets a consumer detect a gap (dropped event) or a reorder, which `timestamp` cannot. |
| `attemptId` | Optional. Groups the events of one attempt when a run is retried, so a replay of attempt 2 is distinguishable from attempt 1. |

`eventId` and `seq` are stamped by `createEventStamper()` in
`@agentkit/core` ([`packages/core/src/events.ts`](../packages/core/src/events.ts)),
never by whoever constructs the event draft — see
[`docs/architecture.md`](architecture.md#event-flow) for how `seq`
stays unbroken across host-driven retries.

### `CONTRACT_VERSION` policy

```ts
export const CONTRACT_VERSION = "0.1.0";
```

([`packages/contracts/src/version.ts`](../packages/contracts/src/version.ts))

Deliberately independent of the npm package version: a packaging-only
release bumps `package.json` and leaves `CONTRACT_VERSION` alone; any
breaking change to a DTO bumps this. The policy is **additive changes
only** within a major — new optional fields, new warning codes, new event
types are non-breaking; removing or repurposing an existing field is not.

## Warning codes

`AiRunWarningCode` is the recognized vocabulary for `run.warning.data.code`
([`packages/contracts/src/run-events.ts`](../packages/contracts/src/run-events.ts)).
The wire schema keeps `code` an open `Type.String()` for forward-compat; the
TS type narrows it to the union below while still accepting any string.

| Code | Emitted by | Meaning |
|---|---|---|
| `tool_call_cap` | core (`runs/run-loop.ts`) | A turn produced more tool calls than `maxToolCallsPerIteration`; the excess was truncated. Also the `errorCode` on the `run.tool.failed` events for the skipped calls. |
| `tool_call_unparseable` | core provider client, with a run-loop fallback | The provider reported `finish_reason=tool_calls` but no usable tool call could be reconstructed (truncated or garbled tool block). |
| `truncated` | core (`runs/run-loop.ts`) | `finish_reason=length` — the answer was cut off. |
| `max_iterations` | core (`runs/run-loop.ts`) | `maxToolIterations` was reached without a final answer. |
| `sse_parse` | core (`providers/openai-compatible.ts`) | Malformed SSE lines were dropped; the response is likely incomplete. |
| `empty_response` | host (`turn/turn-runner.ts`) | The model returned no visible content and no tool calls, even after recovery passes. Not produced by core. |
| `emulated_tool_call` | host (`turn/turn-runner.ts`, `turn/emulated-tool-call.ts`) | The model wrote a JSON-shaped tool call as text instead of calling a real tool, so nothing ran. Not produced by core. |

**Removed, never emitted**: `duplicate_loop`, `stall`, `tool_cap`,
`timeout`. These were declared in an earlier iteration as reserved for
detectors that were never implemented; they are documented here so a
consumer does not go looking for code that emits them.

## The tool envelope

Two related shapes, both in
[`packages/contracts/src/tool.ts`](../packages/contracts/src/tool.ts):

`AiToolResult<T>` — what a tool's `execute()` returns:

```ts
{ ok, data, sources, warnings, truncated, limits, modelData?, summary?, status? }
```

`AiToolEnvelope` — the balanced, model-facing shape the run-loop builds from
a result via `buildEnvelope()` and feeds back as the `role: "tool"` message
content:

```ts
{ ok, status: "ok" | "error" | "partial", summary?, warnings, truncated, data }
```

**The slim-vs-full rule**: `envelope.data` is `result.modelData ?? result.data`
— the model sees the slim payload when a tool provides one, falling back to
the full `data` otherwise. The full `data` always stays available in the
`run.tool.succeeded` event's `resultJson`, so a UI or a later audit can read
it — the model just never has to spend context on it.

The same slim/full split holds at persistence: `TurnRunner.projectEvent`
persists `event.data.modelResultJson ?? event.data.resultJson` as the
`role: "tool"` message's content (what gets replayed into the model's
context on every later turn), while the full payload stays only on the
event log
([`packages/host/src/turn/turn-runner.ts`](../packages/host/src/turn/turn-runner.ts)).

`status: "partial"` survives even when `ok: false` — see the "partial wins"
invariant in [`docs/architecture.md`](architecture.md#loop-invariants).

## `run.usage` — per-call delta semantics

Token accounting is reported **per provider call**, not once per run — a run
makes several calls (every tool round-trip is another), so summing across
calls is the consumer's job
([`packages/contracts/src/run-events.ts`](../packages/contracts/src/run-events.ts)):

- `callId` — identifies one provider call; stable across that call's
  events, distinct between calls of the same run. Minted per call by
  `OpenAiCompatibleClient.streamChat` via `newCallId()`
  ([`packages/core/src/ids.ts`](../packages/core/src/ids.ts)).
- **Dedup key**: `callId` (+ `attempt`, for a retried request billed again
  under the same call).
- `source` — `"stream"` when scraped off a streaming chunk, `"response"`
  when read from a non-streaming response body.
- `finalForCall` — true on the last usage event for this `callId`/`attempt`,
  so a consumer knows the numbers are settled.
- `step` — the run-loop iteration the call belongs to. The provider client
  has no loop context and stamps `0`; `runChat()` re-stamps it with the
  actual iteration number as the event passes through
  (`packages/core/src/runs/run-loop.ts`, the `run.usage` case).

Token fields (`promptTokens`, `completionTokens`, `totalTokens`) are
optional throughout: `OpenAiCompatibleClient` only emits `run.usage` when
the server actually reported usage (it sets `stream_options: { include_usage:
true }` on every streamed request to ask for it, but not every
OpenAI-compatible server honors that).

## Generic kinds: `AiSourceRef<K>` / `AiContextBinding<K>`

Both `AiSourceRef` (a citation on a tool result) and `AiContextBinding` (an
object pinned into a run's context) have a `kind` field that is an **open
string on the wire** and a **type parameter in TypeScript**:

```ts
export type AiSourceRef<K extends string = string> = Omit<
  Static<typeof AiSourceRefSchema>, "kind"
> & { kind: K };
```

([`packages/contracts/src/source-ref.ts`](../packages/contracts/src/source-ref.ts),
mirrored in
[`packages/contracts/src/context-binding.ts`](../packages/contracts/src/context-binding.ts))

AgentKit is domain-agnostic: what a host can cite or bind is the host's
business, not the framework's. A host narrows the field by declaring its own
union and passing it as the type parameter — for example, an EDA host might
declare:

```ts
type EdaSourceKind =
  | "design" | "schematic" | "pcb" | "net" | "part"
  | "library-component" | "symbol" | "footprint"
  | "file" | "tool" | "external";

type EdaSourceRef = AiSourceRef<EdaSourceKind>;
```

That list is an example only — it is not shipped by this package, and no
EDA-specific vocabulary is hard-coded here. `AiContextBinding`'s `role`
(`primary` | `reference` | `comparison`) and `status`
(`active` | `missing` | `stale`) stay closed unions, because those *are*
framework-level concepts.

## REST v1 surface

[`packages/contracts/src/rest.ts`](../packages/contracts/src/rest.ts) is the
versioned HTTP surface as **types and JSON Schemas only** — there is no
server and no client in this repository (see
[`docs/non-goals.md`](non-goals.md)). It exists so the shapes an adapter
will serialize are reviewable and validatable before anything serves them.

- `REST_API_VERSION` (`"v1"`) is the URL-visible version, distinct from
  `CONTRACT_VERSION`: an additive DTO field bumps the contract version and
  leaves the URL alone, because a client written against `/v1` keeps working.
- `REST_ROUTES` is the route table as data — 17 operations keyed by name,
  `{ method, path }` each, `as const satisfies Readonly<Record<string,
  RouteDef>>` — so a router, a client generator, and the documentation
  cannot drift apart by transcription. Two carry header semantics no path
  expresses: `submitMessage` requires an `Idempotency-Key` (it creates a run
  and two messages, and a retried POST without one duplicates the turn), and
  `streamRun` is SSE resuming from `Last-Event-ID`, whose value is an
  `AiRunEvent.eventId` — which is why `eventId` and `seq` are required base
  fields rather than decoration.

**DTOs are projections of host records, not copies.** Each one mirrors a
record in [`packages/host/src/ports/`](../packages/host/src/ports/) with the
orchestrator's internals removed — leases and fencing tokens, queue
bookkeeping (`priority`, `availableAt`, `attemptCount`, `poisonCount`), the
host-shaped `envelope`/`operations` body of a proposal, the `operationId` and
`revisionAtCreate` behind idempotency and staleness. Those exist so the
system can recover from a crash; publishing them would freeze private
mechanics into a public contract. Every omission is documented on the DTO
that makes it.

Three unions are **mirrored rather than imported** — `RunStatusDto`,
`ProposalStatusDto`, `RiskLevelDto` — because contracts sits below host and
cannot depend on it. They restate `RunStatus`, `ProposalStatus`, and
`RiskLevel` verbatim and must be kept in step by hand.

Where a DTO carries something this package already defines, it embeds the
existing schema by reference rather than re-declaring it: `MessageDto` embeds
`AiToolCallSchema`, `ToolEventDto` embeds `AiSourceRefSchema` and
`AiToolStatusSchema`, `ToolDefinitionDto` *is* `AiToolDefinition` (the
definition a model is shown and the one a client lists are the same
document), and an SSE frame (`RunEventFrameDto`) is an `AiRunEvent`.

Errors are RFC 7807 `application/problem+json` (`ProblemDetailsDto`) on every
route, with one AgentKit extension member: `code`, the stable machine-readable
code host errors already carry (`lease_lost`, `duplicate_action_id`,
`revision_conflict`, …). A client branches on `code`; `type`/`title` are for
humans.

## TypeBox as the single source of truth

Every wire DTO is declared once as a `<Name>Schema` TypeBox value; its
TypeScript type is `Static<typeof <Name>Schema>` under the name the type has
always had
([`packages/contracts/src/index.ts`](../packages/contracts/src/index.ts)).
`packages/contracts/src/schemas.ts` re-exports every schema value as one
enumerable barrel, separate from the type-only barrel, so validation/codegen
tooling can import runtime values without pulling in the type surface. Every
schema is Ajv-compatible — `packages/contracts/tests/schemas-compile.test.ts`
compiles each one, and `packages/contracts/tests/golden-validate.test.ts`
validates every event in the committed golden traces
(`packages/testing/src/golden/traces/*.json`) against `AiRunEventSchema`.

Three documented divergences exist where a TypeScript-level nicety outruns
what `Static<>` can express — each is a hybrid: most fields come straight
from `Static<typeof Schema>`, and the divergent field is re-declared over it:

| Type | Divergence |
|---|---|
| `AiToolResult<T>` | `data` is generic in `T`; the schema types it `Type.Unknown()`. |
| `AiSourceRef<K>` / `AiContextBinding<K>` | `kind` is a type parameter; the schema types it an open string. |
| `AiRunWarningEvent` | `data.code` narrows to `AiRunWarningCode \| (string & {})`; the schema keeps it `Type.String()` for forward-compat. |
| `AiRunEvent` | Spelled out as a union rather than `Static<typeof AiRunEventSchema>`, so the `run.warning` member carries the layered `AiRunWarningEvent` type instead of the schema's raw `code: string`. |

One exemption from "TypeBox is the source of truth" itself:
`AiJsonSchemaObject`
([`packages/contracts/src/json-schema.ts`](../packages/contracts/src/json-schema.ts))
is a hand-written interface, not derived from a schema. It is a *meta-type*
— it describes the JSON Schema documents a tool author writes for
`AiToolDefinition.inputSchema`/`outputSchema` — and encoding a schema-of-schemas
recursively in TypeBox would buy nothing over a plain, documented interface.
`AiJsonSchemaObjectSchema` is the TypeBox handle used to embed the shape in
another schema: a `Type.Unsafe` over "any JSON object" that carries
`AiJsonSchemaObject` as its static type.
