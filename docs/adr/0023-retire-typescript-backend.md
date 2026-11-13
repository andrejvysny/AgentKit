# ADR 0023: Retire the TypeScript backend

Status: accepted (2026-11-09). Supersedes the "frozen packages" arrangement in
`docs/architecture.md` and the TypeScript backend described by ADRs 0001-0014.
Ships as AgentKit `0.8.0`.

## Context

The Rust engine owns every production path: durable store, task execution,
sessions, governance, providers, MCP client and server, REST/SSE server and the
coding tool pack. `@agentkit/core`'s `runChat()` and `runAgentLoop()` already
execute on Rust only, and no live package, example or script imports the
TypeScript backend (ADR 0021 decoupling step, commit `0f1fd83`).

Six TypeScript packages stayed behind as frozen libraries, about 42k lines of
source (about 93k with tests): `@agentkit/host`, `@agentkit/adapters-memory`,
`@agentkit/adapters-sqlite`, `@agentkit/runner-local`, `@agentkit/transport-http`
and `@agentkit/mcp-client`. They served two purposes: public API surface for
consumers on the `0.5`-`0.7` shape, and differential witnesses against which the
Rust behaviour was proven. They were still being extended (reference fixes,
parity cells), which kept a second implementation of every rule alive, kept
SQLite native-driver tooling (`better-sqlite3`, `node-gyp`) in the install path,
and carried release machinery (candidate snapshots, inverse patches) for the
`0.6`/`0.7` candidates.

Nothing has been published to npm and the `@agentkit` scope is unverified, so
the break reaches no registry consumer.

## Decision

- Delete the six packages. The Rust engine, reached through `@agentkit/native`
  and the REST v1 + SSE contract, is the only backend.
- Differential testing against the TypeScript implementation ends. The Rust
  fixtures (`rust/fixtures`) and Rust conformance suites are the specification
  from here on.
- Ship the API break as `0.8.0`. Only the umbrella `agentkit` carries the release
  version; `CONTRACT_VERSION` stays `0.7.0` because the wire contract did not
  change.
- TypeScript keeps: `@agentkit/contracts`, `@agentkit/client`,
  `@agentkit/react`, the `@agentkit/core` facade and JS provider lane,
  `@agentkit/native` (plus the `@agentkit/native-*` platform packages),
  `@agentkit/testing` and the `agentkit` umbrella.
- Remove tooling that only served the deleted packages or the `0.6`/`0.7`
  candidate flow: `snapshot-release`, `scripts/release/foundation*`,
  `qualify-candidates`, `publication`, `source-proof`, `fixture-projection`,
  `qualify-package`, `source-digest`, the SQLite durability and conformance
  scripts, and the Electron/SQLite consumer qualification. The native lockstep
  release lane (`scripts/release/native-*`) stays.
- Re-baseline the public API reports (`docs/rust/api`) to the remaining 13
  entry points.

## Consequences

- Removed umbrella subpaths and their replacements:

  | Removed | Replacement |
  | --- | --- |
  | `agentkit/host` | Rust engine via `agentkit/native` (`NativeStore`, `createService`) |
  | `agentkit/transport-http` | Rust REST/SSE server (`service.startHttpServer`), called with `agentkit/client` |
  | `agentkit/adapters-memory` | `NativeStore` on a temporary store file |
  | `agentkit/adapters-sqlite`, `agentkit/adapters-sqlite-node` | `NativeStore` (Rust SQLite store) |
  | `agentkit/runner-local`, `agentkit/runner-local/filesystem` | Rust worker in the native service; managed skills via `NativeManagedContent` |
  | `agentkit/mcp-client` | Rust MCP client in the engine, configured through the MCP REST routes |

- The umbrella no longer depends on `@modelcontextprotocol/*` and no longer
  lists `better-sqlite3` as an optional peer.
- Consumers on the removed subpaths cannot upgrade without recomposing their
  host on `agentkit/native`. The migration guides carry a 0.8 delta
  (`docs/migration/onemind.md`, `docs/migration/openpcb.md`).
- Rust behaviour is no longer cross-checked against a TypeScript oracle.
  Regressions are caught by Rust tests, the exported contract fixtures
  (`bun scripts/export-rust-fixtures.ts --check`) and the client/react suites
  that run against the Rust HTTP fixture.
- Design documents (`docs/ports.md`, `docs/contracts.md`, `docs/roadmap.md`,
  ADRs 0001-0014) still describe the TypeScript shape; they are historical and
  bannered. The deleted source stays recoverable from commit `a31dcb1`
  (`git show a31dcb1:packages/host/src/...`).

## Migration notes

- Replace `TurnRunner` + store + runner composition with
  `NativeStore.open(path)` and `store.createService({ providerConfig, profiles })`;
  serve REST/SSE with `service.startHttpServer(...)` and keep calling it through
  `createAgentKitClient`. `examples/desktop-host` is the reference wiring.
- Existing TypeScript-written SQLite files are imported by the Rust legacy
  import path (`agentkit-store-legacy-import`); there is no TypeScript reader.
- Replace `better-sqlite3` packaging in Electron with packaging the matching
  `@agentkit/native-<platform>` addon for the Electron runtime.
- Conformance suites in `agentkit/testing` now run against the native store.
