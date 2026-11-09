# agentkit

The single installable package for AgentKit: every live `@agentkit/*` package
(`contracts`, `client`, `react`, `core`, `testing`, `native`), built and exposed
as subpath imports of one `agentkit` package. No `@agentkit/*` scope, no
internal dependency wiring for a consumer to get right — one install, one
version, explicit runtime entry points.

0.8.0 removed the frozen TypeScript backend subpaths (`host`, `mcp-client`,
`transport-http`, `adapters-memory`, `adapters-sqlite`, `adapters-sqlite-node`,
`runner-local`, `runner-local/filesystem`); the Rust engine behind
`agentkit/native` is the only backend. See
[ADR 0023](../../docs/adr/0023-retire-typescript-backend.md).

## Install

The current source is the unpublished `0.8.0` release. No release tag was
published here; the earlier `0.5.0` source version was prepared metadata, not a
published artifact. Install an exact qualified tarball; do not use the private
workspace root on `#master` as a package.

```sh
npm install /absolute/path/agentkit-VERSION.tgz
# or: bun add /absolute/path/agentkit-VERSION.tgz
```

Retain the artifact SHA-256 and corresponding qualification evidence. See
[DEVELOPING.md](../../DEVELOPING.md) for the qualification procedure. Publication remains a separate approved action. The `agentkit/native`
subpath resolves its addon from the `@agentkit/native-*` platform packages
pinned at this exact version; until the gated npm lane has published that
version, those optional dependencies resolve to nothing, `agentkit/native`
fails loudly on first use, and every other subpath works unchanged.

## Root import

`import "agentkit"` (no subpath) resolves to `@agentkit/contracts` only — the
wire DTOs and JSON Schemas every other subpath already depends on. It exists
so a bare `import "agentkit"` does not fail with
`ERR_PACKAGE_PATH_NOT_EXPORTED`; it is not a re-export of everything below.
Reach for a subpath for anything else — `agentkit/core`, `agentkit/native`, etc.

## Subpaths

Subpaths, each resolving to that package's public barrel:

| Subpath                     | What                                                  |
| ---------------------------- | ------------------------------------------------------ |
| `agentkit/contracts`         | Wire DTOs and JSON Schemas (TypeBox).                   |
| `agentkit/client`             | Typed REST v1 + SSE client: every operation, auto-resuming run streams. |
| `agentkit/react`              | Headless React hooks over the client. Needs the optional `react` peer. |
| `agentkit/core`               | Pure, in-process chat-with-tools loop (`runChat`).      |
| `agentkit/testing`             | Mocks, fixtures, golden run-event traces, conformance suites. |
| `agentkit/native`                | The Rust runtime through the Node-API binding: `NativeStore`, session service, REST/SSE + MCP servers from Rust, JS tool/provider callbacks, coding tools (`registerCodingTools`). Loads the prebuilt platform addon for your os/cpu. |

```ts
import { runChat } from "agentkit/core";
import { NativeStore } from "agentkit/native";
```

`agentkit/native` is the production backend: `agentkit/core`'s `runChat`
executes on the Rust runtime and **never** falls back to a TypeScript
engine — if the platform addon is missing (unsupported os/cpu, or the npm
lane has not published for your version yet), the first call fails loudly
with `the native binding failed to load`. Prebuilt addons ship as
`optionalDependencies` for `linux-x64-gnu`, `linux-arm64-gnu`,
`darwin-arm64`, `darwin-x64`, and `win32-x64-msvc`; installation never
builds Rust code.

React >=18 is an optional peer; consumers install it explicitly when they use
`agentkit/react`. Electron must rebuild and package the native addon for its own
runtime/ABI. The Node fake-provider host example proves HTTP/SSE wiring,
shutdown, and file reopen without any paid provider credentials.

## Developing AgentKit itself

This package is generated, not hand-written — `scripts/build-umbrella.mjs`
in the repo root assembles `dist/` from the six source packages' own
builds. If you're working on AgentKit rather than just consuming it, see
the repo root's [`DEVELOPING.md`](https://github.com/andrejvysny/AgentKit/blob/master/DEVELOPING.md)
for the local-iteration workflows (npm link, tsconfig path overlay) and the
release ritual.
