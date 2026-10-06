# agentkit

The single installable package for AgentKit: every `@agentkit/*` package
(`contracts`, `client`, `react`, `core`, `host`, `testing`, `mcp-client`,
`transport-http`, `mcp-server`, `adapters-memory`, `adapters-sqlite`,
`runner-local`), built and exposed as subpath imports of one `agentkit` package.
No `@agentkit/*` scope, no internal dependency wiring for a consumer to get
right — one install, one version, explicit runtime entry points.

## Install

The current source is the unpublished `0.7.0` Responses candidate; `0.6.0` is
the separately captured migration foundation candidate. Install only an exact
qualified artifact and verify its digest. No release tag was published here.

No installable release tag was found in the 2026-10-06 audit. The earlier `0.5.0`
source version was prepared metadata, not a published artifact. Install an exact
qualified tarball; do not use the private workspace root on `#master` as a
package.

```sh
npm install /absolute/path/agentkit-VERSION.tgz
# or: bun add /absolute/path/agentkit-VERSION.tgz
# For the Node SQLite subpath:
npm install better-sqlite3@13.0.3
```

Retain the artifact SHA-256 and corresponding qualification evidence. See
[DEVELOPING.md](../../DEVELOPING.md) for the frozen snapshot and qualification
procedure. Publication remains a separate approved action.

## Root import

`import "agentkit"` (no subpath) resolves to `@agentkit/contracts` only — the
wire DTOs and JSON Schemas every other subpath already depends on. It exists
so a bare `import "agentkit"` does not fail with
`ERR_PACKAGE_PATH_NOT_EXPORTED`; it is not a re-export of everything below.
Reach for a subpath for anything else — `agentkit/core`, `agentkit/host`, etc.

## Subpaths

Subpaths, each resolving to that package's public barrel:

| Subpath                     | What                                                  |
| ---------------------------- | ------------------------------------------------------ |
| `agentkit/contracts`         | Wire DTOs and JSON Schemas (TypeBox).                   |
| `agentkit/client`             | Typed REST v1 + SSE client: every operation, auto-resuming run streams. |
| `agentkit/react`              | Headless React hooks over the client. Needs the optional `react` peer. |
| `agentkit/core`               | Pure, in-process chat-with-tools loop (`runChat`).      |
| `agentkit/host`               | Durable orchestration over `core` (`TurnRunner`, tasks, proposals). |
| `agentkit/testing`             | Mocks, fixtures, golden run-event traces, conformance suites. |
| `agentkit/mcp-client`          | MCP servers bridged into a run as a `ToolSetContributor`. |
| `agentkit/transport-http`      | Fetch-standard REST v1 + SSE handler.                   |
| `agentkit/mcp-server`          | The host's tools exposed AS an MCP server over streamable HTTP. |
| `agentkit/adapters-memory`      | Map-backed `AssistantStore` for tests and local dev.    |
| `agentkit/adapters-sqlite`      | Durable `bun:sqlite` `AssistantStore`. **Bun only.**     |
| `agentkit/adapters-sqlite-node` | Durable `better-sqlite3` storage: `NodeSqliteAssistantStore` and `NodeSqliteMcpServerConfigStore`. Node >=22; explicit optional native peer. |
| `agentkit/runner-local`         | Single-process `TaskRunner`.                            |

```ts
import { runChat } from "agentkit/core";
import { TurnRunner } from "agentkit/host";
import { MemoryAssistantStore } from "agentkit/adapters-memory";
```

`agentkit/adapters-sqlite` preserves the Bun `bun:sqlite` entry point.
`agentkit/adapters-sqlite-node` is a separate Node entry point and requires
`better-sqlite3@^13.0.3`, whose engine requires Node >=22. Neither the root
barrel nor browser client/React graphs import either SQLite driver. The umbrella's
Node >=20 engine applies to portable subpaths; it does not relax the native
peer's Node requirement.

React >=18 and better-sqlite3 ^13.0.3 are optional peers. Consumers install the
peer they use explicitly. Electron must rebuild and package the native addon for
its own runtime/ABI; Node CommonJS bundle checks alone do not establish Electron runtime
support. Exact-tarball qualification can additionally execute a pinned Electron
binary in RunAsNode mode, record ABI/Node-API and binary hashes, and check
`npm rebuild` plus reopen. Driver 13.0.3 uses Node-API 10 prebuilds; no
per-Electron source recompilation is claimed by that lifecycle check. The Node fake-provider host example proves HTTP/SSE wiring, shutdown,
and file reopen without any paid provider credentials.

## Developing AgentKit itself

This package is generated, not hand-written — `scripts/build-umbrella.mjs`
in the repo root assembles `dist/` from the twelve source packages' own
builds. If you're working on AgentKit rather than just consuming it, see
the repo root's [`DEVELOPING.md`](https://github.com/andrejvysny/AgentKit/blob/master/DEVELOPING.md)
for the local-iteration workflows (npm link, tsconfig path overlay) and the
release ritual.
