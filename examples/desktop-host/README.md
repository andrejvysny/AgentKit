# @agentkit/example-desktop-host

A runnable composition root on the NATIVE backend: `@agentkit/native` —
`NativeStore` + `NativeSessionService` + the Rust REST v1 + SSE server + the
Rust MCP server — plus an OpenAI-compatible provider lane and two sample
tools, wired together and served over real HTTP with `Bun.serve`. Every
runtime decision (storage, durable queue, worker, governance authority,
REST/SSE, MCP) executes in Rust; the TypeScript here is the embedding recipe
and the loopback façade.

Not a package other code imports — `private: true`, nothing published. It
exists to be read (`src/wiring.ts` is the wiring recipe, step by step) and to
be run.

## Run it

```sh
bun install        # from the repo root, once
cd examples/desktop-host
bun run start       # or: bun src/main.ts
```

No environment variable is required — it boots with a `default` provider
seeded from the `openai-compatible` preset pointing at
`http://127.0.0.1:8000/v1`, which nothing has to be listening on for the
server itself to come up. `GET /v1/version` never touches the provider at
all; submitting a chat message against an unreachable one fails that one run,
not the process.

Env vars, all optional:

| Var | Default | What it does |
| --- | --- | --- |
| `AGENTKIT_DB` | `./agentkit.sqlite` | native store file path (the native store's own database identity — the TypeScript adapter's sqlite file is a different store and is not migrated) |
| `AGENTKIT_HOST` | `127.0.0.1` | bind address. **Loopback on purpose** — see the warning below |
| `AGENTKIT_PORT` | `8787` | HTTP port |
| `AGENTKIT_PROVIDER_KIND` | `openai-compatible` | a kind from `@agentkit/core`'s `AI_PROVIDER_PRESETS` (`openai`, `openrouter`, `lmstudio`, `omlx`, `ollama`, `openai-compatible`) — only read on first boot, when no provider is seeded yet |
| `AGENTKIT_BASE_URL` | the kind's preset `defaultBaseUrl` | provider base URL |
| `AGENTKIT_MODEL` | the kind's preset `defaultModel` | model id |
| `AGENTKIT_API_KEY` | unset | provider API key — crosses into the Rust provider lane exactly once via `NativeSessionService.registerProviderConnection` (the service options only ever carry the connection id, never the key) |
| `AGENTKIT_MCP_COMMAND` | unset | a stdio MCP server command; when set, its tools are bridged in via the native client (`NativeStore.mcpConnect` + `registerMcpTools`) |
| `AGENTKIT_MCP_ARGS` | unset | space-separated args for that command |
| `AGENTKIT_MCP_SERVER_TOKEN` | unset | when set, this host's own tools are ALSO served **as** an MCP server at `/mcp`, behind this bearer token (the Rust MCP server, `NativeSessionService.startMcpServer`) — see below |

> **This example wires no client-facing `authenticate`.** The façade holds a
> private bearer token toward the Rust backend, so every route is open to
> whatever can reach the socket, including the chat routes (which spend the
> seeded provider's API key). That is why `main.ts` binds `127.0.0.1`
> explicitly instead of taking `Bun.serve`'s default, which is every
> interface. Set `AGENTKIT_HOST` only together with a real authentication
> layer in front of `src/wiring.ts`'s façade; `main.ts` prints a warning at
> boot when the bind address is not loopback.

To point it at a local [Ollama](https://ollama.com):

```sh
AGENTKIT_PROVIDER_KIND=ollama AGENTKIT_MODEL=llama3.2 bun run start
```

The provider is seeded **once** — on the first boot against a given
`AGENTKIT_DB`. Deleting the store file resets everything (the seeded provider,
chats, runs) and the next boot seeds fresh from whatever env is set then.

## curl walkthrough

The server mounts the contract under `basePath: "/api/agentkit"` (see
`src/wiring.ts`'s step 5) — every route below is `/api/agentkit/v1/...`.

```sh
BASE=http://localhost:8787/api/agentkit

# 1. Create a chat.
CHAT_ID=$(curl -s -X POST "$BASE/v1/chats" \
  -H 'content-type: application/json' -d '{}' | jq -r .id)

# 2. Submit a message. Idempotency-Key is REQUIRED on this route — this is
#    the one write that creates three records (task, user message, assistant
#    placeholder) at once, and a retry without a key would look like a second
#    "send".
RUN=$(curl -s -X POST "$BASE/v1/chats/$CHAT_ID/messages" \
  -H 'content-type: application/json' \
  -H 'idempotency-key: my-first-message' \
  -d '{"content":"Say hi, then call example_now."}')
echo "$RUN" | jq .
RUN_ID=$(echo "$RUN" | jq -r .runId)

# 3. Follow the run as an SSE stream until it reaches a terminal event
#    (run.completed / run.failed / run.cancelled).
curl -N "$BASE/v1/runs/$RUN_ID/stream"

# 4. List messages — the assistant's answer is in the placeholder step 2
#    already returned the id for (SubmitMessageResponse.assistantMessageId).
curl -s "$BASE/v1/chats/$CHAT_ID/messages" | jq .
```

Two sample tools are always available (see `src/tools.ts`) — ask the model to
use them:

- `example_echo({ text })` → `{ echoed: text }`
- `example_now({})` → `{ now: <ISO-8601 timestamp from the injected Clock> }`

## Serving this host's tools over MCP

The `AGENTKIT_MCP_COMMAND` bridge above points *inward* — someone else's MCP
tools, brought into this host's runs. `AGENTKIT_MCP_SERVER_TOKEN` points the
other way: this host's own tools (`example_echo`, `example_now`, plus any
bridged ones), offered to an outside MCP client.

```sh
AGENTKIT_MCP_SERVER_TOKEN=$(openssl rand -hex 32) bun run start
# [agentkit] mcp server: http://127.0.0.1:8787/mcp (bearer token required)
```

Point a client at it:

```jsonc
{
  "mcpServers": {
    "agentkit-desktop-host": {
      "type": "http",
      "url": "http://127.0.0.1:8787/mcp",
      "headers": {
        "Authorization": "Bearer <the AGENTKIT_MCP_SERVER_TOKEN you set>"
      }
    }
  }
}
```

Or check it by hand:

```sh
# No token — 401, with no body to learn anything from.
curl -i -X POST http://127.0.0.1:8787/mcp

# A rebound Host — refused before the MCP SDK is asked to do anything.
curl -i -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $AGENTKIT_MCP_SERVER_TOKEN" \
  -H "Host: evil.com"
```

Unlike the REST routes, this endpoint is **not** open when it is served:
there is no unauthenticated mode, the bearer set is compared by constant-time
digest, sessions are principal-bound per token, the `Host` header must be
loopback (an allow list), and crate session/body/batch/concurrency/deadline
bounds hold on every request. Write-effect tools are still hidden —
`writesEnabled` defaults to `false`, and this example ships none anyway.
Two reductions are documented, not hidden: a remote call runs WITHOUT the
run-scoped plan-mode/ask guard or a durable execution receipt (the engine has
no standalone tool-dispatch entry point), and there is no per-chat session
pinning header — sessions scope per bearer principal, per token. The native
MCP surface is documented in
[`docs/rust/native-binding-milestone.md`](../../docs/rust/native-binding-milestone.md)
and `packages/native/src/mcp.ts`.

Leaving `AGENTKIT_MCP_SERVER_TOKEN` unset does not serve the route at all.

## What's wired, and where

| Concern | Package | File |
| --- | --- | --- |
| Storage, sessions, durable queue, worker, governance | `@agentkit/native` (`NativeStore`, `NativeSessionService`) | `src/wiring.ts` |
| REST v1 + SSE | `@agentkit/native` (`service.startHttpServer` — the Rust transport) | `src/wiring.ts` (started) / `src/main.ts` (façade mounted on the public port) |
| Provider lane (real boot) | `@agentkit/native` (`registerProviderConnection` + `providerConfig` — HTTP/SSE in Rust) | `src/wiring.ts` |
| Provider lane (tests) | `@agentkit/core` client interface + `@agentkit/testing` (`MockProviderClient`) over the JS `setProvider` callback | `src/wiring.ts` |
| Provider presets | `@agentkit/core` (`getPresetByKind`) | `src/wiring.ts` |
| Sample tools | this example's own descriptors + `registerTool` runs | `src/tools.ts` |
| MCP bridge (optional) | `@agentkit/native` (`store.mcpConnect` + `registerMcpTools`) | `src/wiring.ts` |
| MCP server (optional) | `@agentkit/native` (`service.startMcpServer`) | `src/wiring.ts` (started) / `src/main.ts` (façade mounted at `/mcp`) |
| Managed integrations (opt-in recipe) | `@agentkit/mcp-client` (OAuth connectors) + `@agentkit/host` (templates, skills) — a retained TS slice: the binding has no OAuth and no managed-content services | `src/managed-integrations.ts` |

One registration list is the single source of truth: the descriptors handed
to `registerTool` are what the Rust executor can call, what the MCP server
offers remote clients, and what the façade's `GET /v1/tools` advertises — so
what the route shows, what a turn runs, and what an outside client sees
cannot drift. (The native HTTP server exposes no tool-catalogue port yet —
`DesktopBackend::open` wires none — which is exactly why the catalogue route
is the façade's; see the module header of `src/wiring.ts`.)

## Tests

```sh
bun test tests   # from this directory, or: bun test examples/desktop-host
```

`tests/smoke.test.ts` boots the exact same `buildApp` wiring `main.ts` uses,
with a temp-dir native store file and a scripted `MockProviderClient` (over
the binding's JS provider lane) standing in for the real database and the
network, then mounts the same loopback façade `main.ts` gives `Bun.serve` on
an ephemeral port (`:0`) — and drives it over real HTTP end to end, through
the Rust REST v1 + SSE engine: create a chat (which the façade creates as an
agent session), submit a message with an `Idempotency-Key`, follow the SSE
stream to `run.completed`, read the answer back, and (separately) round-trip
the `example_echo` tool. It cleans up its temp directory and server on every
run.

## Node fake-provider counterpart

`node-host.mjs` is a separate minimal composition root using published
`agentkit/*` imports and `NodeSqliteAssistantStore`. The existing Bun sample is
unchanged. This counterpart binds only loopback on an ephemeral port, uses a
scripted local provider, and requires no API key or remote service.

Copy `node-host.mjs` and `node-main.mjs` to a clean consumer directory, then:

```sh
npm init -y
npm install /absolute/path/agentkit-VERSION.tgz better-sqlite3@13.0.3
node node-main.mjs
```

Node >=22 is required by the native peer. The command prints the actual origin.
Use the same `/v1/chats` and `/v1/chats/:id/messages` REST/SSE routes shown above,
without the Bun sample's `/api/agentkit` prefix. `SIGINT` or `SIGTERM` stops the
server and worker, disposes contributors, and closes SQLite. Reusing
`AGENTKIT_DB` reopens the same file. This fixture has no authentication and
therefore offers no non-loopback bind option.

`exerciseNodeHost(dbPath)` creates a chat with the published typed client,
submits a message, follows the real SSE stream to `run.completed`, reads the
fake answer, shuts down, and verifies the chat after reopening the file. The
exact-tarball qualification script copies and runs this fixture in both clean
npm and Bun consumer projects; it does not import monorepo source.

This is a generic Node host and an Electron-style CommonJS packaging exercise.
The example itself does not implement Electron UI, PCB operations, OAuth, or
restricted DevKit integration. The package qualification script can separately
run a pinned actual Electron binary in RunAsNode mode, load the CommonJS native
external, record ABI/Node-API/binary hashes, and reopen before and after an npm
rebuild lifecycle check. That does not qualify a renderer or packaged installer. Consumers must rebuild
and package `better-sqlite3` for their actual Electron runtime before claiming
that qualification.
