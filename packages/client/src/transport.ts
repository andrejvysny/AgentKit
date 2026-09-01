/**
 * Request plumbing: URL building, header assembly, one `fetch` call.
 *
 * PATHS ARE NOT TRANSCRIBED. Every URL comes from `REST_ROUTES` in
 * `@agentkit/contracts` — the same table `@agentkit/transport-http` compiles its
 * router from — so a renamed segment breaks both sides at once instead of
 * leaving the client asking for a path the server stopped serving. It is the one
 * value (as opposed to type) this package imports, and the reason it depends on
 * contracts at runtime at all.
 *
 * Nothing here touches a Node built-in: `fetch`, `URL`, `TextDecoder` and
 * `crypto` are all standard in browsers, Node ≥ 20 and Bun, which is what lets
 * the same client run in an Electron renderer and in a CLI.
 */
import { REST_ROUTES, type RestOperation } from "@agentkit/contracts";
import { errorForResponse } from "./errors.js";

/** What a caller may add to any single request. */
export interface RequestOptions {
  /** Aborts the request; the rejection is whatever `fetch` rejects with. */
  signal?: AbortSignal;
  /**
   * Extra headers for this call, merged over the client-wide `headers()`.
   *
   * The content type and the idempotency key are applied AFTER these: they are
   * decided by the operation, not by the caller, and a request that lost its
   * `Idempotency-Key` to a stray header spread would duplicate a turn.
   */
  headers?: Record<string, string>;
}

/** A request that mints or replays an `Idempotency-Key`. */
export interface IdempotentRequestOptions extends RequestOptions {
  /**
   * The key to send. Absent, the client mints one with `crypto.randomUUID()`
   * and returns it, so a retry of a timed-out call can reuse it verbatim.
   */
  idempotencyKey?: string;
}

/**
 * A write's answer plus the key that produced it.
 *
 * The key comes back because it is the only thing that makes the retry safe: a
 * `submitMessage` whose response never arrived MUST be retried with the SAME
 * key, and a client that let the transport mint one internally would have
 * nothing to retry with.
 */
export interface IdempotentResult<T> {
  result: T;
  idempotencyKey: string;
}

export type HeaderSource = () =>
  | Record<string, string>
  | Promise<Record<string, string>>;

/**
 * The `fetch` this client will accept — structurally, not `typeof fetch`.
 *
 * `typeof fetch` is not one type: Bun's carries a `preconnect` member and the
 * browser's does not, so an option typed that way rejects the plain
 * `async (url, init) => …` that every wrapper, mock and tracing shim actually
 * is. What this client calls fetch WITH is a URL string and a `RequestInit`, and
 * that — nothing wider — is what it asks of an implementation.
 */
export type FetchLike = (
  input: string,
  init?: RequestInit,
) => Promise<Response>;

export interface AgentKitClientOptions {
  /**
   * Where the API is mounted, INCLUDING any base path the server was mounted
   * under — `http://127.0.0.1:8787` or `https://host/api`. The `/v1/...` half
   * comes from the route table; a trailing slash here is ignored.
   */
  baseUrl: string;
  /**
   * The `fetch` to call. Defaults to the global one. A caller supplies its own
   * to add retries, tracing, or (in Electron) a session-scoped fetch — and the
   * tests in this package use it to sever a stream mid-flight.
   */
  fetch?: FetchLike;
  /**
   * Headers for EVERY request, resolved per call rather than captured once:
   * an access token that expires mid-session must be re-read, not remembered.
   * Async so a caller can refresh one before returning it.
   */
  headers?: HeaderSource;
}

/** Path (`:param`) and query values for one call. */
export interface CallTarget {
  path?: Record<string, string>;
  query?: Record<string, string | number | undefined>;
}

export interface CallInput extends CallTarget {
  body?: unknown;
  /** Sent as `Idempotency-Key` when present. */
  idempotencyKey?: string;
  options?: RequestOptions;
  /** Overrides `Accept`; the SSE routes ask for `text/event-stream`. */
  accept?: string;
  /** Skip the ok-check and body parse; `streamRun` reads the body itself. */
  raw?: boolean;
}

/** The shared state one client instance carries. */
export interface Transport {
  readonly baseUrl: string;
  request(operation: RestOperation, input?: CallInput): Promise<Response>;
  json<T>(operation: RestOperation, input?: CallInput): Promise<T>;
  /** 204/empty-bodied routes. Resolves once the response is consumed. */
  empty(operation: RestOperation, input?: CallInput): Promise<void>;
}

export function createTransport(options: AgentKitClientOptions): Transport {
  const baseUrl = options.baseUrl.replace(/\/+$/, "");
  const doFetch = options.fetch ?? globalThis.fetch.bind(globalThis);
  const headerSource = options.headers;

  async function request(
    operation: RestOperation,
    input: CallInput = {},
  ): Promise<Response> {
    const route = REST_ROUTES[operation];
    const url = buildUrl(baseUrl, route.path, input);

    const headers: Record<string, string> = {
      accept: input.accept ?? "application/json",
      ...(headerSource === undefined ? {} : await headerSource()),
      ...(input.options?.headers ?? {}),
    };
    if (input.body !== undefined) headers["content-type"] = "application/json";
    if (input.idempotencyKey !== undefined) {
      headers["idempotency-key"] = input.idempotencyKey;
    }

    const response = await doFetch(url, {
      method: route.method,
      headers,
      ...(input.body === undefined ? {} : { body: JSON.stringify(input.body) }),
      ...(input.options?.signal === undefined
        ? {}
        : { signal: input.options.signal }),
    });

    if (input.raw === true) return response;
    if (!response.ok) throw await errorForResponse(response, route.method, url);
    return response;
  }

  return {
    baseUrl,
    request,
    async json<T>(operation: RestOperation, input?: CallInput): Promise<T> {
      const response = await request(operation, input);
      return (await response.json()) as T;
    },
    async empty(operation: RestOperation, input?: CallInput): Promise<void> {
      const response = await request(operation, input);
      // Drained rather than ignored: an undrained body keeps a keep-alive
      // connection pinned in some runtimes, and a 204 has nothing to lose.
      await response.arrayBuffer().catch(() => undefined);
    },
  };
}

/**
 * `/v1/chats/:chatId/messages` + `{ chatId }` → an absolute URL.
 *
 * Path values are percent-encoded, ids included: an id is an opaque string this
 * contract never promised to be URL-safe, and the router on the other side
 * decodes `%2F` back to a slash precisely so one containing a separator survives
 * the round trip. Query values that are `undefined` are omitted rather than sent
 * as the string `"undefined"`.
 */
export function buildUrl(
  baseUrl: string,
  routePath: string,
  target: CallTarget,
): string {
  const path = routePath.replace(
    /:([A-Za-z0-9_]+)/g,
    (_match, name: string) => {
      const value = target.path?.[name];
      if (value === undefined) {
        throw new TypeError(
          `Missing path parameter \`${name}\` for ${routePath}.`,
        );
      }
      return encodeURIComponent(value);
    },
  );

  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(target.query ?? {})) {
    if (value === undefined) continue;
    query.set(key, String(value));
  }
  const search = query.toString();
  return `${baseUrl}${path}${search === "" ? "" : `?${search}`}`;
}

/**
 * A fresh idempotency key.
 *
 * `crypto.randomUUID()` where it exists, and `crypto.getRandomValues` where it
 * does not — which is not hypothetical: browsers withhold `randomUUID` outside a
 * secure context, so a page served over plain HTTP on a LAN would otherwise
 * throw on the first submit. Both are WebCrypto; neither is a dependency.
 */
export function newIdempotencyKey(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6]! & 0x0f) | 0x40;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  const hex = [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
