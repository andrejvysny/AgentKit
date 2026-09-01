# ADR 0011 — Tool governance: namespaces, guard chain, dispose, `ToolCatalog` (P7)

**Status:** accepted, implemented (2026-09-01; guard fail-closed hardened
2026-09-02)
**Contract impact:** Additive under `CONTRACT_VERSION` `0.4.0` —
`AiToolErrorData` gains optional `phase`/`retryable`; no version bump per
policy.

## Problem

[`docs/roadmap.md`](../roadmap.md)'s P7 named the goal and marked it a
prerequisite gate: "Must precede the MCP server package, the remote/trusted
tool bridge, and human approval workflows — all three add tool-facing
surface that this phase's guard chain and namespacing are meant to police."
Before this ADR, `ToolSetContributor` had no namespace concept: two
contributors offering the same tool name would silently let one win
(last-registered), meaning the model could be shown one tool's description
and have its call reach a **different** implementation with the arguments it
wrote for the first. There was no way to hide a tool from a deployment
lacking the feature it operates on, no chat-independent way to list what a
run would receive (`GET /v1/tools` stood at a deliberate 501, per [ADR
0005](0005-http-transport.md)), and no way to override a provider's own —
sometimes wrong — probed tool-calling capability.

## Evidence

OneMind's `tool-catalog.ts`, `tool-guards.ts`, and `ModuleLoader.ts`'s
disposer pattern (dynamically-loaded plugin lifecycle) as the primary
semantics reference; OpenPCB's `provider-store.ts` manual tool-calling
override (`auto|on|off`) as the second. Both target consumers need this
before either's write tools or MCP integrations can be trusted to sit behind
one guard chain rather than each inventing its own.

## Decision

1. **`namespace` is now required on `ToolSetContributor`** — a bare
   `^[a-z][a-z0-9_-]*$` token. Deliberately attribution and reservation, not
   a prefix: tool names are never mechanically rewritten (a `ns__` rename
   would change the name every existing tool is called by, and
   `TOOL_NAME_PATTERN` already forbids dots). `agentkit`, `chat`, and `mcp`
   are **reserved** — refused at staging unless the contributor also sets
   the framework-internal `privileged: true` (only `@agentkit/mcp-client`
   sets it, for `mcp`).
2. **Cross-contributor collisions fail the whole staging closed**
   (`tool_name_collision`, naming both namespaces) — never last-write-wins.
   A duplicate **within** one contributor stays lenient (logged, that one
   tool dropped): there is no ambiguity of ownership to fail over when only
   one party is offering the name twice.
3. **`ToolGuard` chain**, two hooks checked at different moments:
   `isVisible(ctx)` at **registry staging** (a hidden tool is never
   advertised, never spends context on every turn, never invites the model
   to keep trying something it will always be refused) and `canExecute(ctx)`
   at **call time** (for state that moves *within* a run — a lock taken, a
   budget spent, a binding gone stale). A `canExecute` refusal becomes an
   `ok: false` tool result (`errorCode: "tool_guard_refused"`, `phase:
   "guard"`), never a thrown error — the run completes and the
   `tool_call_id` stays balanced. Guards compose with **AND**; an absent
   hook is "no opinion," never "allow"; order is not significant.
4. **Contributor lifecycle: `dispose?()`** (optional), called once per
   contributor by `TurnRunner.disposeContributors()` at shutdown —
   idempotent, and a throw is logged rather than rethrown, since a shutdown
   that gives up halfway leaks more than the error it reported.
5. **`SettingsStore.toolCalling: "auto" | "on" | "off"`** (default `auto`)
   as the manual override atop a provider's **probed** `toolCalling`
   capability: `on` stages tools even when the probe said unsupported
   (probing is a heuristic against someone else's server, and a wrong
   `false` would otherwise leave a capable model permanently toolless), `off`
   stages none at all and never even calls the contributors.
6. **Structured tool errors**: `AiToolErrorData` gains optional `phase`
   (`"validation" | "guard" | "execution"`) and `retryable`, set only where
   the producer actually knows them — absent means "unrecorded," never
   "false."
7. **`ToolCatalog` port + `createContributorToolCatalog`**: enumerate tools
   *without* running a turn — what `GET /v1/tools` needed and had been
   answering 501 without ([ADR 0005](0005-http-transport.md)'s deliberate
   blocker, now closeable). The default implementation runs the **same**
   `stageRegistry` the turn runner does, so namespace checks, guards and
   unbound pruning all apply identically and the catalogue cannot drift from
   what a run actually receives — a second, disagreeing enumeration path
   would be worse than the 501 it replaces, because it would look
   authoritative. **Definitions only** (no `execute`) — handing out an
   executable here would open a second, unguarded, unlogged call path beside
   the run loop's own.
8. **Hardened the next day, from a Phase B/C verifier pass: a guard that
   throws now fails closed, per tool, not per run.** An `isVisible` that
   throws hides that **one** tool (logged as a warning); a `canExecute` that
   throws refuses that **one** call, reported to the model as `phase:
   "guard"` with the fixed reason `"guard error"` — the thrown message is
   deliberately never forwarded, since a stack trace or a connection string
   from a broken guard is not something to hand the model, and a guard's
   *intended* reason otherwise reaches the model verbatim. Scoping the
   failure to the tool being judged means a guard broken for one tool costs
   one tool, while a guard broken for all of them empties the registry —
   which is loud, and therefore noticeable, by design.

## Alternatives considered

- **`namespace` as a mechanical name prefix (`ns__toolName`).** Rejected:
  renames the identity every existing tool is called by, a bigger and
  riskier change than reservation-plus-attribution for the actual problem
  (collision detection and reserved-prefix policing).
- **Resolve cross-contributor collisions last-write-wins**, matching how
  within-contributor duplicates are handled. Rejected: a within-contributor
  duplicate has one owner deciding to drop its own tool; a cross-contributor
  collision has two owners, and picking a winner shows the model a
  description that reaches the *loser's* implementation.
- **A guard throw treated as an allow (fail open).** Rejected outright: a
  policy hook's entire job is to keep something away from the model, and the
  safe reading of "the hook could not answer" is "it said nothing" — which
  for a keep-away hook means keep away, not let through.
- **A guard throw failing the whole registry or run rather than the one
  tool.** Rejected: loud in the wrong way — one broken guard among many
  contributors would silently stop every *other* contributor's tools from
  working too, a bigger blast radius than the actual defect.

## Consequences

- [`docs/roadmap.md`](../roadmap.md)'s P7 gate is closed — the MCP server
  package ([ADR 0013](0013-serving-surfaces.md)), a future remote/trusted
  tool bridge, and human approval workflows can build on a policed namespace
  plus guard chain rather than each inventing one.
- `GET /v1/tools` answers 200 wherever a `ToolCatalog` is wired
  (`examples/desktop-host` does) — the concrete blocker [ADR
  0005](0005-http-transport.md) recorded is now closeable per host rather
  than permanently 501.
- Every `ToolGuard` a host writes from this point on must assume it can be
  asked to answer, throw, and answer again for the **same** tool across a
  run's lifetime, and that a thrown answer is read as a refusal it never
  actually reasoned about.

## Out of scope (deliberate)

A chat-independent tool-enumeration UI or picker (`ToolCatalog` is the port;
a host builds its own surface over it); per-user or per-tenant tool
visibility beyond what a `ToolGuard` can express; wiring write-capable MCP
tools through the proposal pipeline (still open, per [ADR
0004](0004-mcp-client.md)).
