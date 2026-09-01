# @agentkit/example-desktop-host

A runnable composition root: `SqliteAssistantStore` + `SingleProcessTaskRunner`
+ an OpenAI-compatible provider + two sample tools + the REST/SSE transport,
wired together the way `packages/host/README.md`'s embedding sketch describes
and served over real HTTP with `Bun.serve`.

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
| `AGENTKIT_DB` | `./agentkit.sqlite` | sqlite file path |
| `AGENTKIT_HOST` | `127.0.0.1` | bind address. **Loopback on purpose** — see the warning below |
| `AGENTKIT_PORT` | `8787` | HTTP port |
| `AGENTKIT_PROVIDER_KIND` | `openai-compatible` | a kind from `@agentkit/core`'s `AI_PROVIDER_PRESETS` (`openai`, `openrouter`, `lmstudio`, `omlx`, `ollama`, `openai-compatible`) — only read on first boot, when no provider is configured yet |
| `AGENTKIT_BASE_URL` | the kind's preset `defaultBaseUrl` | provider base URL |
| `AGENTKIT_MODEL` | the kind's preset `defaultModel` | model id |
| `AGENTKIT_API_KEY` | unset | provider API key — stored in the in-memory `SecretStore`, never inline on the provider config (see `InMemorySecretStore` in `src/wiring.ts`) |
| `AGENTKIT_MCP_COMMAND` | unset | a stdio MCP server command; when set, its tools are bridged in via `@agentkit/mcp-client` |
| `AGENTKIT_MCP_ARGS` | unset | space-separated args for that command |
| `AGENTKIT_MCP_SERVER_TOKEN` | unset | when set, this host's own tools are ALSO served **as** an MCP server at `/mcp`, behind this bearer token — see below |

> **This example wires no `authenticate` and no `authorize`.** Every route is
> open to whatever can reach the socket, including `POST /v1/providers` (which
> stores provider API keys) and the chat routes (which spend them). That is
> why `main.ts` binds `127.0.0.1` explicitly instead of taking `Bun.serve`'s
> default, which is every interface. Set `AGENTKIT_HOST` only together with
> real `authenticate`/`authorize` in `src/wiring.ts`'s `RestHandlerDeps`;
> `main.ts` prints a warning at boot when the bind address is not loopback.

To point it at a local [Ollama](https://ollama.com):

```sh
AGENTKIT_PROVIDER_KIND=ollama AGENTKIT_MODEL=llama3.2 bun run start
```

The provider is seeded **once** — on the first boot against a given
`AGENTKIT_DB`. Deleting the sqlite file resets everything (providers, chats,
runs, proposals) and the next boot seeds fresh from whatever env is set then.

## curl walkthrough

The server mounts the contract under `basePath: "/api/agentkit"` (see
`src/wiring.ts`'s step 11) — every route below is `/api/agentkit/v1/...`.

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
        "Authorization": "Bearer <the AGENTKIT_MCP_SERVER_TOKEN you set>",
        // Optional. Pins the session to one chat, so tools that depend on a
        // chat's bindings see that chat. Read ONCE, at initialize.
        "x-agentkit-chat": "chat_...."
      }
    }
  }
}
```

Or check it by hand:

```sh
# No token — 401, with no body to learn anything from.
curl -i -X POST http://127.0.0.1:8787/mcp

# A rebound Host — 403, before the MCP SDK is asked to do anything.
curl -i -X POST http://127.0.0.1:8787/mcp \
  -H "Authorization: Bearer $AGENTKIT_MCP_SERVER_TOKEN" \
  -H "Host: evil.com"
```

Unlike the REST routes, this endpoint is **not** open when it is served: there
is no unauthenticated mode, the bearer token is compared in constant time,
`Host` must be loopback (any port), and a session's chat scope is fixed at
initialize rather than read per call. Write tools would still be hidden —
`writesEnabled` defaults to `false`, and this example ships none anyway. The
full security model is in
[`packages/mcp-server/README.md`](../../packages/mcp-server/README.md).

Leaving `AGENTKIT_MCP_SERVER_TOKEN` unset does not serve the route at all.

## What's wired, and where

| Concern | Package | File |
| --- | --- | --- |
| Storage | `@agentkit/adapters-sqlite` | `src/wiring.ts` |
| Task queue | `@agentkit/runner-local` | `src/wiring.ts` |
| Provider client | `@agentkit/core` (`OpenAiCompatibleClient`) | `src/wiring.ts` |
| Provider presets | `@agentkit/core` (`getPresetByKind`) | `src/wiring.ts` |
| Secrets | tiny in-memory `SecretStore` (this example's own) | `src/wiring.ts` |
| Sample tools | this example's own `ToolSetContributor` | `src/tools.ts` |
| MCP bridge (optional) | `@agentkit/mcp-client` | `src/wiring.ts` |
| MCP server (optional) | `@agentkit/mcp-server` | `src/wiring.ts` (built) / `src/main.ts` (mounted at `/mcp`) |
| Tool guards | `@agentkit/host` (`ToolGuard`) — one `toolGuards` array, shared | `src/wiring.ts` |
| Turn execution | `@agentkit/host` (`TurnRunner`, `ChatTurnExecutor`) | `src/wiring.ts` |
| HTTP + SSE | `@agentkit/transport-http` (`serveRest`) | `src/main.ts` |

Three places stage the same contributors — `TurnRunner`, the `GET /v1/tools`
catalogue, and the MCP server's tool source — so `src/wiring.ts` defines
`toolGuards` ONCE and passes that same array to all three. The array is empty
here (this example has no policy to enforce); the point is the single source. A
host that guards the runner but not the catalogue advertises tools its own turns
refuse, and one that guards neither the MCP source hands an outside client a
tool set its chats do not have.

## Tests

```sh
bun test tests   # from this directory, or: bun test examples/desktop-host
```

`tests/smoke.test.ts` boots the exact same `buildApp` wiring `main.ts` uses,
with a temp-dir sqlite file and a scripted `MockProviderClient` standing in
for the real database and the network, served on an ephemeral port (`:0`) —
then drives it over real HTTP: create a chat, submit a message with an
`Idempotency-Key`, follow the SSE stream to `run.completed`, read the answer
back, and (separately) round-trip the `example_echo` tool. It cleans up its
temp directory and server on every run.
