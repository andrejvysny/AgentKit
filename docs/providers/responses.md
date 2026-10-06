# Responses provider

`ResponsesClient` implements the existing `AiProviderClient` interface. `runChat`
remains the only provider/tool loop. The host injects transport, credentials and
account selection; core does not run OAuth or use private endpoints.

The public Responses endpoint is `https://api.openai.com/v1/responses`. The
ChatGPT account profile uses `store: false`, `stream: true`, a full input array and
instructions, with no `previous_response_id`. Profile and protocol checked
2026-10-06 against official [models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference),
[preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations),
[stream events](https://developers.openai.com/api/reference/resources/responses/streaming-events)
and [conversation state](https://developers.openai.com/api/docs/guides/conversation-state).

## Trusted transport

```ts
import {
  ResponsesClient,
  type TrustedResponsesTransport,
} from "@agentkit/core";

const transport: TrustedResponsesTransport = {
  identity: {
    providerId: "provider",
    protocol: "responses",
    connectionId: "connection",
    generation: 1,
  },
  authKind: "chatgpt-account",
  async request(input) {
    input.signal?.throwIfAborted();
    if (input.operation === "models") {
      return Response.json({ models: [{
        slug: "example-model", display_name: "Example", visibility: "list",
      }] });
    }
    const event = { type: "response.completed", response: {
      id: "example-response", status: "completed", output: [{
        type: "message", id: "example-message", role: "assistant",
        status: "completed", content: [{ type: "output_text", text: "Hello" }],
      }],
    } };
    return new Response(`data: ${JSON.stringify(event)}\n\n`, {
      headers: { "content-type": "text/event-stream" },
    });
  },
};
const client = new ResponsesClient({ id: "provider", transport });
```

This mock is self-contained. A production transport must resolve credentials
from **the supplied `input.connectionIdentity`**, never a globally selected
account. Identity contains no secrets. `generation` is a nonnegative safe
integer changed when credentials or account binding change. Each client freezes
its identity and rejects transport identity drift before dispatch. Transport must
honor the signal during headers and streaming. There is no paid-account fallback.
`beforeRequest` reserves the shared durable budget immediately before dispatch.

Account catalogs use `models`, retain server order and include only
`visibility: "list"`; labels come from `display_name`, IDs from `slug`. Catalogs
have no global account cache.

## Supported requests

Text, user URL/data images, function calls and exact-ID function results are
supported. Function tools use the fixed `agentkit` namespace with verbatim names
and `strict: false`. Optional properties, unions, enums and
`additionalProperties` remain intact. Unsupported schemas, tool names, custom or
native tools, audio/video/file content and unresolved image references fail
before transport. Account-profile temperature and output-token overrides also
fail explicitly rather than being ignored.

The adapter consumes the full stream through EOF after a successful terminal
response. Tool calls are emitted only after validation and private continuation
persistence. Interleaved arguments and final-output reconciliation do not
produce duplicate calls. Late failure, incomplete response, malformed framing,
unknown tools and abrupt EOF prevent tool dispatch. Successful usage appears
once per call; terminal usage followed by failure remains partial, non-final
usage. Provider errors are categorized and sanitized.

## Private continuation and host replay

Direct core callers provide `continuationScope: { chatId, branchId }` and an
async `onContinuation` callback. Reuse the returned state for the next pass.
Callback completion is awaited before any tool dispatch. A required missing
continuation, changed scope or changed history prefix fails before transport.

Continuation version 1 binds provider, protocol, connection, generation, model,
chat and branch. Its SHA-256 digest covers the normalized exact message prefix.
The envelope is bounded to 1 MiB and 1024 input items. Opaque encrypted reasoning
and raw provider items stay in private state, never visible text or error data.

`TurnRunner` requires the optional private `AssistantStore.continuations` port
for Responses. The memory and SQLite adapters implement it. Each run binds its
identity before tool staging; fenced state writes require a live run lease.
Retries prefer current-run state; later turns use the latest active ancestor.
Branch changes, account/model changes, missing state, history truncation,
system-prompt drift and crash gaps between private persistence and canonical
projection fail closed. SQLite state survives close/reopen. No continuation
fields are added to public provider DTOs, messages, task events or REST bodies.

Responses mode stores each completed assistant pass as an internal canonical
record. Its visible placeholder carries the final answer and a display marker;
provider history skips that placeholder. Tool-pass text, exact calls/results,
original finals and correction write-backs remain in chronological history.
Empty-answer retry appends an explicit follow-up while retaining prior state.
Other provider clients keep their existing projection behavior.

Coverage uses generic mock streams and reference stores. OpenPCB consumer fixture
acceptance remains pending; these mocks do not claim consumer provenance.
