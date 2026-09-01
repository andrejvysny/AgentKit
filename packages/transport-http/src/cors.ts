/**
 * CORS: opt-in, and off by default.
 *
 * A default-on CORS layer is a default-open one — the interesting deployments of
 * this adapter are a desktop app talking to `127.0.0.1` and a service behind a
 * gateway that already sets these headers, and neither wants a second opinion
 * baked into the handler. Configure `deps.cors` and every response carries the
 * headers; leave it out and no response is touched at all, byte for byte as
 * before this file existed.
 *
 * What is deliberately NOT here: `Access-Control-Allow-Credentials`. Cookie- or
 * TLS-credentialled cross-origin requests are a decision with a blast radius
 * (they make CSRF reachable, and forbid the `*` origin outright), and a host
 * that wants them should say so at its own edge rather than get them from a
 * transport package's options bag.
 */

export interface RestCorsOptions {
  /**
   * Origins allowed to read responses. `"*"` allows any — the right setting for
   * a public read-only API and the wrong one for anything else. A list is
   * matched by exact string comparison against the request's `Origin` header
   * (that is what the header is: scheme + host + port, no path, no trailing
   * slash), because a pattern language here is a pattern language to get wrong.
   */
  origins: string[] | "*";
  /**
   * Request headers a browser may send. Defaults to
   * {@link DEFAULT_ALLOW_HEADERS} — the four this contract actually uses.
   */
  allowHeaders?: string[];
  /**
   * Response headers a browser may read. Empty by default: the contract's
   * bodies carry everything a client needs, so nothing has to be exposed for
   * the API to be usable, and exposing a header nobody reads is surface for
   * free.
   */
  exposeHeaders?: string[];
  /** How long a preflight may be cached. Omitted, no `Access-Control-Max-Age` is sent. */
  maxAgeSeconds?: number;
}

/**
 * `Content-Type` (every POST body), `Idempotency-Key` (required on
 * `submitMessage`), `Last-Event-ID` (SSE resume) and `Authorization` (whatever
 * `deps.authenticate` reads). A host with a custom auth header adds it here.
 */
export const DEFAULT_ALLOW_HEADERS: readonly string[] = Object.freeze([
  "Content-Type",
  "Idempotency-Key",
  "Last-Event-ID",
  "Authorization",
]);

/**
 * The `Access-Control-Allow-Origin` value for this request, or `null` when the
 * request has no origin or the origin is not allowed.
 *
 * A non-matching origin is NOT an error: the request is served exactly as it
 * would be without CORS configured, and the browser — which is the only party
 * CORS protects — refuses to hand the response to the page. Answering 403 here
 * would break every non-browser client that happens to send an `Origin` header
 * while protecting nobody.
 */
export function allowedOrigin(
  req: Request,
  cors: RestCorsOptions | undefined,
): string | null {
  if (cors === undefined) return null;
  const origin = req.headers.get("origin");
  if (origin === null) return null;
  if (cors.origins === "*") return "*";
  return cors.origins.includes(origin) ? origin : null;
}

/**
 * The headers every CORS-eligible response carries.
 *
 * `Vary: Origin` goes on unconditionally, including the `*` case: whether the
 * response is `*` or an echoed origin is a function of configuration that can
 * change, and a cache that stored one origin's answer for another is the one
 * CORS bug that survives a fix to the code.
 */
export function corsResponseHeaders(
  origin: string,
  cors: RestCorsOptions,
): Record<string, string> {
  const expose = cors.exposeHeaders ?? [];
  return {
    "access-control-allow-origin": origin,
    vary: "Origin",
    ...(expose.length === 0
      ? {}
      : { "access-control-expose-headers": expose.join(", ") }),
  };
}

/**
 * Re-emit a response with the CORS headers added.
 *
 * A new `Response` around the SAME body rather than a mutation, because a
 * `Response`'s headers are only reliably writable depending on how it was
 * constructed, and this function is handed responses from four different places
 * (routes, the problem builders, `authenticate`'s verbatim answer, the SSE
 * stream). Passing `res.body` through preserves streaming — the SSE response is
 * re-wrapped, not buffered — and nothing else about the response changes.
 */
export function withCorsHeaders(
  res: Response,
  headers: Record<string, string>,
): Response {
  const merged = new Headers(res.headers);
  for (const [name, value] of Object.entries(headers)) {
    merged.set(name, value);
  }
  return new Response(res.body, {
    status: res.status,
    statusText: res.statusText,
    headers: merged,
  });
}

/**
 * The preflight answer: 204, no body, and the three questions a browser asked.
 *
 * `Access-Control-Allow-Methods` comes from the route table — the methods this
 * exact path serves — rather than from a fixed list, so a preflight tells the
 * truth about the path instead of about the API in general.
 */
export function preflightResponse(
  origin: string,
  allowMethods: readonly string[],
  cors: RestCorsOptions,
): Response {
  const allowHeaders = cors.allowHeaders ?? DEFAULT_ALLOW_HEADERS;
  return new Response(null, {
    status: 204,
    headers: {
      ...corsResponseHeaders(origin, cors),
      "access-control-allow-methods": [...new Set(allowMethods)].join(", "),
      "access-control-allow-headers": allowHeaders.join(", "),
      ...(cors.maxAgeSeconds === undefined
        ? {}
        : { "access-control-max-age": String(cors.maxAgeSeconds) }),
    },
  });
}
