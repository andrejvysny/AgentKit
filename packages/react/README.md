# @agentkit/react

Headless React hooks over [`@agentkit/client`](../client). A conversation with
optimistic sends and streamed deltas, a live run log that survives a dropped
connection, branch switching, the staged-write queue, and the provider settings
list — as five hooks and one provider component that renders nothing.

**Headless means headless.** There is no message list here, no bubble, no
composer, no spinner, no class name and no CSS import. What every consumer of
this framework has in common is the PROTOCOL — submit, stream, reconcile,
branch, approve. What none of them share is the interface, and a component
library at this layer would be a design system three applications have to fight
rather than a dependency they can use.

`react` is a **peer** dependency (`>=18`), and nothing in `src/` imports
`react-dom`: the hooks work in a DOM renderer, in React Native, and in a custom
reconciler.

## Install

Through the umbrella package:

```jsonc
// package.json
"dependencies": {
  "agentkit": "github:andrejvysny/AgentKit#v0.4.0",
  "react": "^19"
}
```

```ts
import { AgentKitProvider, useChat } from "agentkit/react";
```

`react` is an OPTIONAL peer of `agentkit` — the other eleven subpaths have
nothing to do with React, so an installer that only wants `agentkit/host` is not
told it is missing something. Optional also means npm will not install it for
you; a project using these hooks already has React.

Inside this workspace: `@agentkit/react`.

## Quickstart

```tsx
import { createAgentKitClient } from "agentkit/client";
import { AgentKitProvider, useChat } from "agentkit/react";

const client = createAgentKitClient({ baseUrl: "http://127.0.0.1:8787" });

export function App({ chatId }: { chatId: string }) {
  return (
    <AgentKitProvider client={client}>
      <Conversation chatId={chatId} />
    </AgentKitProvider>
  );
}

function Conversation({ chatId }: { chatId: string }) {
  const { messages, status, phase, submit, cancel, error } = useChat(chatId);

  return (
    <>
      {messages
        .filter((m) => m.metadata.internal !== true)
        .map((m) => (
          <article key={m.id} data-role={m.role}>
            {typeof m.content === "string" ? m.content : "[parts]"}
          </article>
        ))}

      {error && <p role="alert">{error.message}</p>}

      <form
        onSubmit={(event) => {
          event.preventDefault();
          const input = new FormData(event.currentTarget).get("q");
          void submit(String(input));
          event.currentTarget.reset();
        }}
      >
        <input name="q" disabled={status === "streaming"} />
        {status === "streaming" ? (
          <button type="button" onClick={() => void cancel()}>
            Stop ({phase})
          </button>
        ) : (
          <button type="submit">Send</button>
        )}
      </form>
    </>
  );
}
```

`submit` resolves when the server ACCEPTS the turn, not when the answer is
finished — a form that awaited the whole answer before clearing its input would
hold the input hostage for the length of the turn. Watch `status`/`phase` for
the rest.

## The hooks

| Hook                     | Renders                                                                                  |
| ------------------------ | ---------------------------------------------------------------------------------------- |
| `useChat(chatId)`        | the chat's active path, plus `submit` / `regenerate` / `editAndResubmit` / `cancel` / `reload` |
| `useRun(runId)`          | one run's event log, live and resuming, plus `drain`                                     |
| `useBranches(messageId)` | a message's siblings, which one is active, and `activate`                                |
| `useProposals(chatId)`   | the staged-write queue, plus `approve` / `reject` / `apply`                               |
| `useProviders()`         | the provider list and model catalogues, plus the five writes                              |

Every hook takes an options object with an optional `client`, which replaces the
provider's client for that hook only — one component talking to a second host
while the rest of the tree talks to the first. It replaces the client, never the
provider.

Every action captures its failure into `error` as an `AgentKitClientError` (or a
plain `Error`) and returns; nothing here throws into a click handler. The one
exception is a hook used OUTSIDE `AgentKitProvider`, which throws during render
with an explanation — a hook that quietly did nothing there would present as
"the chat never loads".

### `useChat`, in detail

A turn is not request/response. `submitMessage` returns in milliseconds with
three ids and no answer; the answer arrives over SSE for the next several
seconds; the durable truth lands in the store as the events are projected. So
this hook renders all three, in order:

1. **Optimistic.** The user message and an empty assistant placeholder are in
   `messages` before `submit`'s first `await`. A branch submit (an edit) also
   truncates the path at the branch point, because the answer that followed the
   old question is not on the new branch.
2. **Streaming.** Deltas are applied to the placeholder as they arrive, by the
   same rule the host's own projector uses (`packages/host/src/turn/projection.ts`):
   deltas accumulate, and a `run.message.completed` overwrites the text only for
   a provider that streamed nothing. At a **pass boundary** — a `retry_pass`
   warning, or a second `run.started`, i.e. the host abandoning that pass and
   asking again — the placeholder is emptied and the run is live again, mirroring
   the host's own reset. `phase` follows the run's LAST pass, so a turn whose
   first pass failed and whose retry answered is `completed`, not `failed`.
3. **Reconciled.** Once the stream closes (the *task* is terminal), one resumed
   `drainRun` pass collects anything appended after it, then `listMessages`
   replaces the whole list with what the server stored. Anything the streaming
   step got wrong survives for at most one round trip.

A failed submit **parks its `Idempotency-Key`**: calling `submit` again with the
same content and the same `parentMessageId` replays that key instead of asking
the question twice, which is what a "send failed — retry?" button needs.
Different content mints a fresh key, because a replayed key against a different
question answers the old one.

`cancel()` asks the server to stop and does **not** tear down the local stream:
the run answers with its own `run.cancelled`, and letting that arrive is what
leaves the hook with a correct final phase and a reconciled list.

`messages` is the contract's `MessageDto[]`, verbatim. Filtering the host's
replay-only records (`metadata.internal === true`) is the caller's job — a hook
that hid them would be making a rendering decision.

## The invalidation model

The hooks are independent by design; the server's state is not. Activating a
branch changes what `listMessages` reports. A finished run can mint a proposal.
So `AgentKitProvider` carries a **change emitter** — a `Map` of `Set`s, about
thirty lines, no dependency — and the hooks talk through it:

| Emits `chat:<id>:changed`                             | Subscribes                              |
| ----------------------------------------------------- | --------------------------------------- |
| `useChat.submit` / `regenerate` (on accept, and again on terminal) | `useChat` (re-lists the path) |
| `useBranches.activate` (after the switch)             | `useBranches` (re-lists the siblings)   |
|                                                       | `useProposals` (re-lists the queue)     |

Each event carries the `origin` of the hook instance that caused it, so a hook
can tell its own echo from somebody else's news and skip a redundant re-read.
Nothing is stored on the bus and no payload travels on a topic: a subscriber's
only correct reaction is to re-read from the server, which is the one source of
truth there is.

The bus belongs to the **provider**, not the module. Two providers are two
buses — the right isolation for a test, and the reason hooks that must see each
other's writes have to be under the same provider.

`useAgentKitEmitter()` exposes it, so an application that writes through
`@agentkit/client` directly can invalidate a chat itself:

```ts
const emitter = useAgentKitEmitter();
await client.forkChat({ chatId }, { fromMessageId });
emitter.emit(chatTopic(chatId));
```

## Server rendering

Safe. No hook touches `window` or `document`, and every read starts in a
`useEffect` — which a server render never runs — so `renderToString` produces
the initial state and makes no requests. There is no `useSyncExternalStore`
subscription to hydrate and no client-only bail-out to configure.

`<StrictMode>` is exercised in the test suite: every effect is idempotent under
React's development double-invoke, streams abort on the discarded mount, and
`useRun` de-duplicates by `eventId` in case one delivered before the abort
landed.

## What this package deliberately does not have

- **Components.** One provider, which renders its children and nothing else.
- **Routing, layout, styling, icons, virtualisation.**
- **A query cache.** TanStack Query is the right answer for an application and
  the wrong answer for a library that must not pick the application's data
  layer. Wrap these hooks in one if you have one.
- **`useAllowances`.** The write-policy allowance routes are moving; a hook over
  a route in flight would be a compatibility promise this package cannot keep
  yet. Call `@agentkit/client` for them.
- **Response validation.** Inherited from `@agentkit/client`, which does not
  re-litigate in the browser what the server validated on the way out.

## Tests

```bash
bun test packages/react     # or: bun run test:react
```

Against the real stack: `@agentkit/transport-http` over the in-memory store, the
real queue and `TurnRunner`, a scripted provider, behind `Bun.serve` on an
ephemeral port. Mocking the client would test the mock — the resume path in
particular only means anything against a server that really replays from
`Last-Event-ID`.

The DOM comes from `@happy-dom/global-registrator`, registered by
`tests/support/dom.ts`, which then puts Bun's own `fetch`, streams, `crypto`,
`AbortSignal` and timers BACK: the registrator replaces every one of them, and
happy-dom's `fetch` would apply its window's same-origin policy to the fixture's
`127.0.0.1:<port>` URL. `bun test` also runs every package in one process, so a
swapped global would follow this suite into the next package's. Import
`./support/dom.js` first in any new test file — before
`@testing-library/react`, whose `screen` binds `document.body` at import time.
