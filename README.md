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

## Packages

| Package | Description | Status |
|---|---|---|
| [`packages/contracts`](packages/contracts) — `@agentkit/contracts` | Wire DTOs and JSON Schemas (TypeBox): run events, tool/provider/prompt shapes, source refs, context bindings, multimodal message content parts, the task-event envelope, and the REST v1 route table + DTOs. | 0.1.0-dev |
| [`packages/core`](packages/core) — `@agentkit/core` | Pure, in-process chat-with-tools loop: provider client, Ajv-backed tool registry, `runChat()`, multimodal content-part mapping for OpenAI-compatible providers. | 0.5.0-dev |
| [`packages/host`](packages/host) — `@agentkit/host` | Durable orchestration over `@agentkit/core`: `TaskStore` + kind-dispatched executors, `TurnRunner` as the `chat.turn` executor, proposal lifecycle, write policy. | 0.1.0-dev |
| [`packages/testing`](packages/testing) — `@agentkit/testing` | Mocks, fixtures, golden run-event traces, and the `AssistantStore` conformance suite. | 0.1.0-dev |
| [`packages/mcp-client`](packages/mcp-client) — `@agentkit/mcp-client` | Optional adapter: bridges MCP servers' tools into a run as a `ToolSetContributor`, on the official `@modelcontextprotocol/sdk`. | 0.1.0-dev |
| [`packages/transport-http`](packages/transport-http) — `@agentkit/transport-http` | Optional adapter: fetch-standard REST v1 + SSE handler serving `packages/contracts/src/rest.ts`, zero framework dependencies. | 0.1.0-dev |
| [`internal/reference-adapters`](internal/reference-adapters) — `@agentkit/reference-adapters` | Reference `AssistantStore` + `TaskRunner` implementations (in-memory, `bun:sqlite`). Workspace-private, never published — dev/test only. | 0.1.0-dev, private |

## Layers

```
internal/reference-adapters  →  @agentkit/host  →  @agentkit/core  →  @agentkit/contracts
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
- [`PROVENANCE.md`](PROVENANCE.md) — how `packages/core` was extracted from
  `@openpcb/ai-core` and relicensed to MIT.

## Development

This is a Bun workspaces monorepo (`packages/*`, `internal/*`).

```sh
bun install
bun test
bun run ci        # install --frozen-lockfile && typecheck && test && build
```

Other useful scripts (see [`package.json`](package.json)): `bun run
typecheck`, `bun run build`, and per-package variants
(`bun run test:core`, `bun run test:host`, `bun run test:contracts`,
`bun run test:testing`, `bun run test:adapters`, `bun run test:mcp-client`,
`bun run test:transport-http`).

Bun is the primary runtime, but every published `@agentkit/*` package must
stay Node-loadable — a `bun:` import anywhere in one passes the whole Bun
suite and breaks the first Node consumer. `scripts/node-smoke.mjs` runs the
built dists (contracts, core, host, mcp-client, transport-http, testing)
under plain Node: Ajv-validating a golden event, driving `runChat` with a
stub provider, loading the host port vocabulary, constructing an mcp-client
manager, serving a request through transport-http, and round-tripping a
golden trace through testing. **Build first** — it reads `dist/`, which is
not checked in:

```sh
bun run build && bun run smoke:node
```

CI runs the same thing in a separate `node-smoke` job, preceded by a grep
that fails on any `bun:` import in any of those six dists.

## License

MIT — see [LICENSE](LICENSE). See [PROVENANCE.md](PROVENANCE.md) for the
history of how this code was extracted and relicensed.
