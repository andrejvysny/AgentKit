# AgentKit

AgentKit is an embeddable AI-assistant framework extracted from OpenPCB's
assistant module. The backend — durable store, sessions, governance, providers,
MCP, REST/SSE — is a Rust workspace (`rust/`) reached from TypeScript through
`@agentkit/native`. The TypeScript packages are the API surface around it:
wire DTOs (`@agentkit/contracts`), a typed REST/SSE client
(`@agentkit/client`), React hooks (`@agentkit/react`), the in-process provider
lane (`@agentkit/core`) and test support (`@agentkit/testing`). The frozen
TypeScript backend was deleted in 0.8.0
([ADR 0023](docs/adr/0023-retire-typescript-backend.md)). See
[`docs/architecture.md`](docs/architecture.md) for the full layer breakdown.

**Status:** pre-release, 0.x. APIs may change without notice.

> **Not published to npm (yet).** The `@agentkit` npm scope is not yet
> verified/owned — nothing under this scope may be published until scope
> ownership is confirmed by the project owner. Hosted workflows were removed;
> qualification now uses [local source-bound gates](DEVELOPING.md#local-gates-no-hosted-ci).
> `bun run qualify:local /absolute/new/outside-directory` retains candidate
> artifacts and evidence without publishing. Current qualification is macOS only;
> other operating systems are deferred. Missing configured runtime execution
> remains blocked. Native migration parity and release readiness are not accepted.
> Publication requires separate authorization and an available reviewed route;
> earlier workflow instructions are historical.

## Installing

Consumers install AgentKit as **one package**, `agentkit`, with subpath
imports. Today that means an exact qualified release tarball (produced and
verified per [`DEVELOPING.md`](DEVELOPING.md)). Registry publication remains
separate work requiring explicit authorization:

```sh
npm install /absolute/path/agentkit-VERSION.tgz   # qualified artifact (current)
# npm install agentkit                            # once the approved npm lane has published
```

`npm install` / `bun install` then works with subpath imports
(`agentkit/core`, `agentkit/client`, `agentkit/native`, …) backed by
a packed `dist/` — no build step for the consumer. `agentkit/native` runs
the whole backend on the Rust runtime through the Node-API binding and pulls
the prebuilt addon for your platform (`linux-x64-gnu`, `linux-arm64-gnu`,
`darwin-arm64`, `darwin-x64`, `win32-x64-msvc`) via os/cpu/libc-gated
optional dependencies — installation never builds Rust code. See
[`packages/agentkit/README.md`](packages/agentkit/README.md) for the full
subpath table and [`DEVELOPING.md`](DEVELOPING.md) for local-iteration
workflows and the release ritual.

For unpublished native candidates, install the retained matching platform
tarball alongside the umbrella tarball. Optional dependencies cannot download
an unpublished platform package from the registry. Qualification verifies this
isolated installation without a Rust toolchain.

## Packages

| Package | Description | Status |
|---|---|---|
| [`packages/contracts`](packages/contracts) — `@agentkit/contracts` | Wire DTOs and JSON Schemas (TypeBox): run events, tool/provider/prompt shapes, source refs, context bindings, multimodal message content parts, the task-event envelope, and the REST v1 route table + DTOs. | 0.1.0-dev |
| [`packages/client`](packages/client) — `@agentkit/client` | Typed REST v1 + SSE client for the contract: one method per route, `streamRun` as an auto-resuming async iterable, derived run phases, problem+json as typed errors. Browser and Node; no dependency but `@agentkit/contracts`. | 0.1.0-dev |
| [`packages/react`](packages/react) — `@agentkit/react` | Headless React hooks over `@agentkit/client`: `useChat` (optimistic send, streamed deltas, reconcile), `useRun`, `useBranches`, `useProposals`, `useProviders`. No components, no styling; `react` is a peer dependency. | 0.1.0-dev |
| [`packages/core`](packages/core) — `@agentkit/core` | Pure, in-process chat-with-tools loop: provider client, Ajv-backed tool registry, `runChat()`, multimodal content-part mapping for OpenAI-compatible providers. | 0.5.0-dev |
| [`packages/testing`](packages/testing) — `@agentkit/testing` | Mocks, fixtures, golden run-event traces, and the `AssistantStore` + `TaskRunner` conformance suites. | 0.1.0-dev |
| [`packages/native`](packages/native) — `@agentkit/native` | Thin Node-API binding over the Rust backend (`rust/crates/*`): durable store, sessions, governance, providers, MCP, HTTP — all in Rust, no logic mirrored in TypeScript. | publishable; npm lane gated |
| [`packages/native-*`](packages/native-linux-x64-gnu) — `@agentkit/native-<platform>` | Prebuilt per-platform addons (`linux-x64-gnu`, `linux-arm64-gnu`, `darwin-arm64`, `darwin-x64`, `win32-x64-msvc`), pulled in automatically as optional dependencies; installation never builds Rust. | publishable; npm lane gated |

## Layers

```
@agentkit/react → @agentkit/client → @agentkit/contracts      (REST/SSE consumers)
@agentkit/core  → @agentkit/native → Rust engine (rust/crates) (in-process backend)
```

The TypeScript packages carry no orchestration logic; the Rust engine owns
every runtime rule. See [`docs/architecture.md`](docs/architecture.md) for the
full layer breakdown, the event flow (`seq`/`eventId` stamping, lease fencing),
the run/attempt/lease model, and the loop invariants.

## Example embedding

[`examples/desktop-host`](examples/desktop-host) is the canonical
single-process composition root: `NativeStore` + native session service +
`startHttpServer` for Rust-owned storage, admission, execution, and REST/SSE,
with sample tools supplied through JS callbacks. Managed provider mode requires
explicit host vault and consent ports; fixed connections and custom provider
callbacks remain explicit extension lanes. A loopback facade supplies the
application route prefix, CORS, and MCP mounting. HTTP smoke tests exercise the
composition end to end. It is a workspace-private example,
not a published package — read it as the reference wiring, not as
something to depend on.

## Runtime support

Every package is **ESM-only** (`"type": "module"`, no CJS build) and requires
**Node ≥20 or Bun ≥1.3** — declared as `engines` in each `package.json`. Bun
is the primary runtime; `@agentkit/contracts` and `@agentkit/core` are also
tested under plain Node (see `node-smoke` below), but the rest of the
toolchain (tests, `bun run ci`) is Bun-only. The backend itself — chat loop,
durable store, orchestration, providers, MCP, coding toolpack — runs on
**Rust** through `@agentkit/native` (Node-API 9; Node ≥22, Bun ≥1.4; see
[`docs/rust/parity.md`](docs/rust/parity.md) for the per-slice status and
[`docs/rust/acceptance.md`](docs/rust/acceptance.md) for the verification
ledger); there is no TypeScript backend fallback.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — the three layers, event
  flow, run/attempt/lease model and recovery, at-least-once stance, loop
  invariants.
- [`docs/contracts.md`](docs/contracts.md) — the event vocabulary, v2 base
  fields, warning codes, tool envelope, `run.usage` semantics, generic
  kinds, TypeBox conventions.
- [`docs/ports.md`](docs/ports.md) — the port catalog (responsibility, key
  invariant per port) and the proposal lifecycle diagram; reference
  implementations are historical (deleted in 0.8.0).
- [`docs/non-goals.md`](docs/non-goals.md) — what this repository
  deliberately does not include yet, and where it will live.
- [`docs/roadmap.md`](docs/roadmap.md) — the sequenced backlog (P1–P5a, P5b,
  P5c, P7, and the distribution/serving-surface work all shipped; P6
  long-term memory is the next open phase; a Later list) this deferred
  surface lands against.
- [`docs/adr/`](docs/adr/) — accepted architecture decision records.
- [`CHANGELOG.md`](CHANGELOG.md) — what changed per contract version, and
  the working method (ADR gating, reference-repo extraction policy,
  verification bar).
- [`docs/migration/`](docs/migration/) — playbooks for migrating an existing
  embedding (OpenPCB, OneMind) onto AgentKit: delete/keep-and-adapt lists,
  port mappings, and data-migration notes.
- [`DEVELOPING.md`](DEVELOPING.md) — installing and iterating on the
  `agentkit` umbrella package (GitHub-tag install, `npm link`, the release
  ritual).
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
(`bun run test:core`, `bun run test:contracts`, `bun run test:client`,
`bun run test:react`, `bun run test:testing`).

Bun is the primary runtime, but every published `@agentkit/*` package must
stay Node-loadable — a `bun:` import anywhere in one passes the whole Bun
suite and breaks the first Node consumer. `scripts/node-smoke.mjs` runs the
built dists (contracts, client, react, core, testing) under plain Node:
Ajv-validating a golden event, constructing a REST client over the whole route
table, driving `runChat` with a stub provider (which must fail honestly without
the native binding), and round-tripping a golden trace through testing. **Build
first** — it reads `dist/`, which is not checked in:

```sh
bun run build && bun run smoke:node
```

The former hosted `node-smoke` job ran this check. Current acceptance uses
the [local gates](DEVELOPING.md#local-gates-no-hosted-ci), including installed
native and umbrella consumers.

## License

MIT — see [LICENSE](LICENSE). See [PROVENANCE.md](PROVENANCE.md) for the
history of how this code was extracted and relicensed.

Managed account connections, OAuth, GitHub/Plane reference connectors, selected
skills and versioned prompt templates are described in the
[connector system guide](docs/connector-system.md), including compatibility and
qualification limits.
