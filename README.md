# AgentKit

AgentKit is an embeddable AI-assistant framework extracted from OpenPCB's
assistant module: a pure, in-process chat-with-tools loop (`@agentkit/core`)
wrapped in a durable orchestration layer (`@agentkit/host`) over a small set
of storage/execution ports, with wire DTOs (`@agentkit/contracts`) and test
support (`@agentkit/testing`) shared across both. See
[`docs/architecture.md`](docs/architecture.md) for the full layer
breakdown and an ASCII diagram.

**Status:** pre-release, 0.x. APIs may change without notice.

> **Not published to npm.** The `@agentkit` npm scope is not yet
> verified/owned — do not publish any package under this scope until scope
> ownership is confirmed. Every package's `package.json` has a
> `publishConfig` ready for that day; none of them are on the registry
> today.

## Installing

Consumers install AgentKit as **one package**, `agentkit`, via a GitHub tag
— no npm registry publish required:

```jsonc
"dependencies": {
  "agentkit": "github:andrejvysny/AgentKit#v0.4.0"
}
```

`npm install` / `bun install` then works with subpath imports
(`agentkit/core`, `agentkit/host`, `agentkit/adapters-sqlite`, …) backed by
a committed `dist/` — no build step for the consumer. See
[`packages/agentkit/README.md`](packages/agentkit/README.md) for the full
subpath table and [`DEVELOPING.md`](DEVELOPING.md) for local-iteration
workflows and the release ritual.

## Packages

| Package | Description | Status |
|---|---|---|
| [`packages/contracts`](packages/contracts) — `@agentkit/contracts` | Wire DTOs and JSON Schemas (TypeBox): run events, tool/provider/prompt shapes, source refs, context bindings, multimodal message content parts, the task-event envelope, and the REST v1 route table + DTOs. | 0.1.0-dev |
| [`packages/client`](packages/client) — `@agentkit/client` | Typed REST v1 + SSE client for the contract: one method per route, `streamRun` as an auto-resuming async iterable, derived run phases, problem+json as typed errors. Browser and Node; no dependency but `@agentkit/contracts`. | 0.1.0-dev |
| [`packages/core`](packages/core) — `@agentkit/core` | Pure, in-process chat-with-tools loop: provider client, Ajv-backed tool registry, `runChat()`, multimodal content-part mapping for OpenAI-compatible providers. | 0.5.0-dev |
| [`packages/host`](packages/host) — `@agentkit/host` | Durable orchestration over `@agentkit/core`: `TaskStore` + kind-dispatched executors, `TurnRunner` as the `chat.turn` executor, proposal lifecycle, write policy. | 0.1.0-dev |
| [`packages/testing`](packages/testing) — `@agentkit/testing` | Mocks, fixtures, golden run-event traces, and the `AssistantStore` + `TaskRunner` conformance suites. | 0.1.0-dev |
| [`packages/adapters-memory`](packages/adapters-memory) — `@agentkit/adapters-memory` | Map-backed `AssistantStore` for tests and local development. No durability, no rollback. | 0.1.0-dev |
| [`packages/adapters-sqlite`](packages/adapters-sqlite) — `@agentkit/adapters-sqlite` | Durable `AssistantStore` over `bun:sqlite` — the production store for a single-process host. **Bun only.** | 0.1.0-dev |
| [`packages/runner-local`](packages/runner-local) — `@agentkit/runner-local` | Single-process `TaskRunner`: claim, execute, heartbeat, classified retry with backoff, dead-letter, recover. | 0.1.0-dev |
| [`packages/mcp-client`](packages/mcp-client) — `@agentkit/mcp-client` | Optional adapter: bridges MCP servers' tools into a run as a `ToolSetContributor`, on the official `@modelcontextprotocol/sdk`. | 0.1.0-dev |
| [`packages/transport-http`](packages/transport-http) — `@agentkit/transport-http` | Optional adapter: fetch-standard REST v1 + SSE handler serving `packages/contracts/src/rest.ts`, zero framework dependencies. | 0.1.0-dev |
| [`packages/mcp-server`](packages/mcp-server) — `@agentkit/mcp-server` | Optional adapter: exposes the host's `ToolCatalog` **as** an MCP server over streamable HTTP — constant-time bearer auth, DNS-rebinding guard, per-session scope, write-tool filtering. | 0.1.0-dev |

## Layers

```
adapters-memory / adapters-sqlite / runner-local  →  @agentkit/host  →  @agentkit/core  →  @agentkit/contracts
```

Each package depends only on the ones to its right. Full diagram, the event
flow (`seq`/`eventId` stamping, lease fencing), the run/attempt/lease model,
and the loop invariants `runChat()` preserves: see
[`docs/architecture.md`](docs/architecture.md).

## Runtime support

Every package is **ESM-only** (`"type": "module"`, no CJS build) and requires
**Node ≥20 or Bun ≥1.3** — declared as `engines` in each `package.json`. Bun
is the primary runtime; `@agentkit/contracts` and `@agentkit/core` are also
tested under plain Node (see `node-smoke` below), but the rest of the
toolchain (tests, `bun run ci`) is Bun-only.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — the three layers, event
  flow, run/attempt/lease model and recovery, at-least-once stance, loop
  invariants.
- [`docs/contracts.md`](docs/contracts.md) — the event vocabulary, v2 base
  fields, warning codes, tool envelope, `run.usage` semantics, generic
  kinds, TypeBox conventions.
- [`docs/ports.md`](docs/ports.md) — the full port catalog (responsibility,
  key invariant, reference adapter per port) and the proposal lifecycle
  diagram.
- [`docs/non-goals.md`](docs/non-goals.md) — what this repository
  deliberately does not include yet, and where it will live.
- [`docs/roadmap.md`](docs/roadmap.md) — the sequenced backlog (P1–P5a
  shipped, P5b–P7 active, and a Later list) this deferred surface lands
  against.
- [`docs/adr/`](docs/adr/) — accepted architecture decision records.
- [`CHANGELOG.md`](CHANGELOG.md) — what changed per contract version, and
  the working method (ADR gating, reference-repo extraction policy,
  verification bar).
- [`PROVENANCE.md`](PROVENANCE.md) — how `packages/core` was extracted from
  `@openpcb/ai-core` and relicensed to MIT.

## Development

This is a Bun workspaces monorepo (`packages/*`).

```sh
bun install
bun test
bun run ci        # install --frozen-lockfile && typecheck && test && build
```

Other useful scripts (see [`package.json`](package.json)): `bun run
typecheck`, `bun run build`, and per-package variants
(`bun run test:core`, `bun run test:host`, `bun run test:contracts`,
`bun run test:client`,
`bun run test:testing`, `bun run test:adapters-memory`,
`bun run test:adapters-sqlite`, `bun run test:runner-local`,
`bun run test:mcp-client`, `bun run test:transport-http`,
`bun run test:mcp-server`).

Bun is the primary runtime, but every published `@agentkit/*` package must
stay Node-loadable — a `bun:` import anywhere in one passes the whole Bun
suite and breaks the first Node consumer. `scripts/node-smoke.mjs` runs the
built dists (contracts, client, core, host, adapters-memory, runner-local,
mcp-client, transport-http, mcp-server, testing) under plain Node:
Ajv-validating a golden event, constructing a REST client over the whole route
table, driving `runChat` with a stub provider, loading
the host port vocabulary, claiming a task out of the in-memory store, driving
that task to `completed` through the local runner's claim loop, constructing an
mcp-client manager, serving a request through transport-http, refusing an
unauthenticated and a DNS-rebound request through mcp-server, and
round-tripping a golden trace through testing. `@agentkit/adapters-sqlite` is the one
exception, Bun-only by construction (`bun:sqlite`) — see its
[README](packages/adapters-sqlite/README.md). **Build first** — it reads
`dist/`, which is not checked in:

```sh
bun run build && bun run smoke:node
```

CI runs the same thing in a separate `node-smoke` job, preceded by a grep
that fails on any `bun:` import in any of those nine dists.

## License

MIT — see [LICENSE](LICENSE). See [PROVENANCE.md](PROVENANCE.md) for the
history of how this code was extracted and relicensed.
