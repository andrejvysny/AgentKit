/**
 * A DOM for `bun test`, without taking Bun's platform away from the rest of the
 * suite.
 *
 * WHY A DOM AT ALL for a package with no components: `renderHook` mounts a real
 * React root, and `react-dom/client` needs a `document` to mount into. There is
 * no headless React renderer left to use instead — `react-test-renderer` is
 * deprecated as of React 19 — so the hooks are exercised the way an application
 * runs them, which is also the only way `<StrictMode>`'s double-invoked effects
 * are exercised at all.
 *
 * WHY THE RESTORE LIST. `@happy-dom/global-registrator` copies EVERY own
 * property of its `Window` onto `globalThis`, and that includes `fetch`,
 * `Response`, `AbortSignal`, `crypto`, `URL` and the streams — replacing Bun's
 * natives with happy-dom's re-implementations. That breaks this suite before it
 * breaks anything else: the fixture below is a real `Bun.serve` on a real
 * socket, `@agentkit/client` reads `response.body` as a `ReadableStream` and
 * passes an `AbortSignal` into `fetch`, and a happy-dom `fetch` applies the
 * window's own same-origin policy to a `127.0.0.1:<port>` URL it was never told
 * about. `bun test` also runs every package's tests in ONE process, so a
 * swapped-out global would follow this file into `@agentkit/transport-http`'s
 * suite.
 *
 * So: snapshot the platform globals, register happy-dom, put the platform ones
 * back. What is left over from happy-dom is the part that was actually wanted —
 * `document`, `window`, the element classes, `navigator`, `location` — and
 * those are inert for every other package here (nothing in this repo branches
 * on `typeof window`).
 *
 * Import this module FIRST in every test file. `assist.actions.source.
 * organizeImports` is off in `biome.json`, so the order written is the order
 * evaluated, and it has to run before `@testing-library/react` — whose
 * `screen` binds `document.body` at import time.
 */
import { GlobalRegistrator } from "@happy-dom/global-registrator";

/**
 * Globals that must stay Bun's after registration.
 *
 * Everything the client, the transport and the in-process server touch, plus
 * the timers and the `MessageChannel` React's scheduler drives its background
 * work with. NOT on the list: `Event`, `EventTarget`, `CustomEvent` and the
 * element classes — react-dom attaches its listeners to happy-dom's `document`
 * and must see happy-dom's event plumbing there.
 */
const PLATFORM_GLOBALS = [
  "fetch",
  "Request",
  "Response",
  "Headers",
  "FormData",
  "Blob",
  "File",
  "ReadableStream",
  "WritableStream",
  "TransformStream",
  "AbortController",
  "AbortSignal",
  "TextEncoder",
  "TextDecoder",
  "crypto",
  "URL",
  "URLSearchParams",
  "WebSocket",
  "structuredClone",
  "btoa",
  "atob",
  "queueMicrotask",
  "setTimeout",
  "setInterval",
  "clearTimeout",
  "clearInterval",
  "MessageChannel",
  "MessagePort",
  "performance",
  "console",
] as const;

const global = globalThis as unknown as Record<string, unknown>;

if (typeof global["document"] === "undefined") {
  const saved = new Map<string, PropertyDescriptor | undefined>();
  for (const key of PLATFORM_GLOBALS) {
    saved.set(key, Object.getOwnPropertyDescriptor(globalThis, key));
  }

  GlobalRegistrator.register({ url: "http://localhost/" });

  for (const [key, descriptor] of saved) {
    if (descriptor === undefined) continue;
    Object.defineProperty(globalThis, key, {
      ...descriptor,
      configurable: true,
    });
  }
}

/**
 * Re-exported so a test file's first import is unambiguously a side effect and
 * not something a lint pass or a refactor can drop as unused.
 */
export const domReady = true;
