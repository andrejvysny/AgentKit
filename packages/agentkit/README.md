# agentkit

The single installable package for AgentKit: every `@agentkit/*` package
(`contracts`, `core`, `host`, `testing`, `mcp-client`, `transport-http`,
`mcp-server`, `adapters-memory`, `adapters-sqlite`, `runner-local`), built and
exposed as subpath imports of one `agentkit` package. No `@agentkit/*` scope, no
internal dependency wiring for a consumer to get right — one install, one
version, ten entry points.

## Install

```jsonc
// package.json
"dependencies": {
  "agentkit": "github:andrejvysny/AgentKit#v0.4.0"
}
```

Then `npm install` or `bun install`. The pinned tag's branch ships a
committed `dist/` — no build step, no `prepare` script, nothing else to
run. Works under both npm and Bun.

## Subpaths

There is no root `agentkit` export — only these ten subpaths, each
resolving to that package's public barrel:

| Subpath                     | What                                                  |
| ---------------------------- | ------------------------------------------------------ |
| `agentkit/contracts`         | Wire DTOs and JSON Schemas (TypeBox).                   |
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
Node ≥20 or Bun ≥1.3.

## Developing AgentKit itself

This package is generated, not hand-written — `scripts/build-umbrella.mjs`
in the repo root assembles `dist/` from the ten source packages' own
builds. If you're working on AgentKit rather than just consuming it, see
the repo root's [`DEVELOPING.md`](https://github.com/andrejvysny/AgentKit/blob/master/DEVELOPING.md)
for the local-iteration workflows (npm link, tsconfig path overlay) and the
release ritual.
