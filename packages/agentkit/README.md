# agentkit

The single installable package for AgentKit: every `@agentkit/*` package
(`contracts`, `client`, `react`, `core`, `host`, `testing`, `mcp-client`,
`transport-http`, `mcp-server`, `adapters-memory`, `adapters-sqlite`,
`runner-local`), built and exposed as subpath imports of one `agentkit` package.
No `@agentkit/*` scope, no internal dependency wiring for a consumer to get
right — one install, one version, twelve entry points.

## Install

```jsonc
// package.json
"dependencies": {
  // No release tag exists yet — `v0.5.0` lands once the hardening tranche
  // ships. Until then, pin a commit SHA, or track the branch with `#master`.
  "agentkit": "github:andrejvysny/AgentKit#master"
}
```

Then `npm install` or `bun install`. The pinned tag's branch ships a
committed `dist/` — no build step, no `prepare` script, nothing else to
run. Works under both npm and Bun.

## Root import

`import "agentkit"` (no subpath) resolves to `@agentkit/contracts` only — the
wire DTOs and JSON Schemas every other subpath already depends on. It exists
so a bare `import "agentkit"` does not fail with
`ERR_PACKAGE_PATH_NOT_EXPORTED`; it is not a re-export of everything below.
Reach for a subpath for anything else — `agentkit/core`, `agentkit/host`, etc.

## Subpaths

Twelve subpaths, each resolving to that package's public barrel:

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
| `agentkit/runner-local`         | Single-process `TaskRunner`.                            |

```ts
import { runChat } from "agentkit/core";
import { TurnRunner } from "agentkit/host";
import { MemoryAssistantStore } from "agentkit/adapters-memory";
```

`agentkit/adapters-sqlite` is built on `bun:sqlite` and only loads under
Bun — every other subpath is plain, portable JavaScript that loads under
Node ≥20 or Bun ≥1.3. `package.json`'s top-level `engines` (`node >=20, bun
>=1.3`) describes the package as a whole, not this one subpath: an installer
targeting Node alone can use every subpath except `agentkit/adapters-sqlite`,
which needs Bun regardless of what `engines` says.

`agentkit/react` is the one subpath with a peer dependency: `react >=18`,
declared OPTIONAL so an installer that only wants `agentkit/host` is not told
it is missing something. Install React yourself if you import the hooks —
npm does not install an optional peer for you.

## Developing AgentKit itself

This package is generated, not hand-written — `scripts/build-umbrella.mjs`
in the repo root assembles `dist/` from the twelve source packages' own
builds. If you're working on AgentKit rather than just consuming it, see
the repo root's [`DEVELOPING.md`](https://github.com/andrejvysny/AgentKit/blob/master/DEVELOPING.md)
for the local-iteration workflows (npm link, tsconfig path overlay) and the
release ritual.
