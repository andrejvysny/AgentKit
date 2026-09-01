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
| `AGENTKIT_PORT` | `8787` | HTTP port |
| `AGENTKIT_PROVIDER_KIND` | `openai-compatible` | a kind from `@agentkit/core`'s `AI_PROVIDER_PRESETS` (`openai`, `openrouter`, `lmstudio`, `omlx`, `ollama`, `openai-compatible`) — only read on first boot, when no provider is configured yet |
| `AGENTKIT_BASE_URL` | the kind's preset `defaultBaseUrl` | provider base URL |
| `AGENTKIT_MODEL` | the kind's preset `defaultModel` | model id |
| `AGENTKIT_API_KEY` | unset | provider API key — stored in the in-memory `SecretStore`, never inline on the provider config (see `InMemorySecretStore` in `src/wiring.ts`) |
| `AGENTKIT_MCP_COMMAND` | unset | a stdio MCP server command; when set, its tools are bridged in via `@agentkit/mcp-client` |
| `AGENTKIT_MCP_ARGS` | unset | space-separated args for that command |

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
| Turn execution | `@agentkit/host` (`TurnRunner`, `ChatTurnExecutor`) | `src/wiring.ts` |
| HTTP + SSE | `@agentkit/transport-http` (`serveRest`) | `src/main.ts` |

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
