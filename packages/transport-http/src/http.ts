/**
 * The small fetch-standard helpers every route shares: JSON out, JSON in, and
 * the two query parameters the contract's list routes accept.
 *
 * Nothing here knows about AgentKit. It exists so a route handler reads as the
 * projection it is, rather than as header bookkeeping with a projection inside.
 */
import { badRequest } from "./problem.js";

/** A success body. `null` bodies are 204, everything else is JSON. */
export function jsonResponse(
  body: unknown,
  status = 200,
  headers?: Record<string, string>,
): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...(headers ?? {}) },
  });
}

/**
 * Parse a request body as JSON.
 *
 * An EMPTY body reads as `{}` rather than as an error: three of the four bodies
 * in this contract are entirely optional fields (`CreateChatRequest`,
 * `ProposalDecisionRequest`), and rejecting `POST /v1/chats` with no body would
 * make the simplest call in the API the one that needs a body to say nothing.
 * A body that is present but malformed, or that is not a JSON object, is a 400
 * — an array or a bare string can never satisfy any request schema here.
 */
export async function readJsonObject(
  req: Request,
  instance: string,
): Promise<
  | { ok: true; value: Record<string, unknown> }
  | { ok: false; response: Response }
> {
  let text: string;
  try {
    text = await req.text();
  } catch (err) {
    return {
      ok: false,
      response: badRequest(
        "invalid_body",
        `Could not read the request body: ${err instanceof Error ? err.message : String(err)}`,
        instance,
      ),
    };
  }
  if (text.trim() === "") return { ok: true, value: {} };
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (err) {
    return {
      ok: false,
      response: badRequest(
        "invalid_body",
        `Request body is not valid JSON: ${err instanceof Error ? err.message : String(err)}`,
        instance,
      ),
    };
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return {
      ok: false,
      response: badRequest(
        "invalid_body",
        "Request body must be a JSON object.",
        instance,
      ),
    };
  }
  // After the parse, not during it: the check is a walk over the value, and it
  // is cheap next to the parse that already happened.
  if (exceedsMaxDepth(parsed)) {
    return {
      ok: false,
      response: badRequest(
        "invalid_body",
        `Request body is nested more than ${MAX_JSON_DEPTH} levels deep.`,
        instance,
      ),
    };
  }
  return { ok: true, value: parsed as Record<string, unknown> };
}

/**
 * Read a positive integer query parameter. Absent is `undefined`; present and
 * unparseable is an error rather than a silent default, because a client that
 * sent `?limit=all` asked a question this server cannot answer and should be
 * told so instead of handed page one.
 */
export function readPositiveInt(
  url: URL,
  name: string,
  max: number = MAX_PAGE_LIMIT,
): { ok: true; value?: number } | { ok: false; message: string } {
  const raw = url.searchParams.get(name);
  if (raw === null) return { ok: true };
  const value = Number(raw);
  if (!Number.isInteger(value) || value <= 0 || value > max) {
    return {
      ok: false,
      message: `Query parameter \`${name}\` must be an integer between 1 and ${max}.`,
    };
  }
  return { ok: true, value };
}

/**
 * Ceiling on every `limit` this surface accepts.
 *
 * Refused rather than clamped: a client that asked for 100000 rows and silently
 * got 1000 pages forever off a `nextCursor` it thinks it has already passed. A
 * 400 says what happened while the client can still fix it.
 */
export const MAX_PAGE_LIMIT = 1000;

/**
 * Deepest JSON nesting `readJsonObject` will hand to a route.
 *
 * `JSON.parse` itself is iterative and survives deep input, but nearly
 * everything downstream of it is not: structural validation walks content
 * parts, `structuredClone` copies metadata bags, and the store serializes them
 * again. A 200-byte body of nothing but `[` is a stack overflow somewhere in
 * that chain, and which frame it lands in depends on the runtime.
 */
const MAX_JSON_DEPTH = 64;

/**
 * Whether anything in `root` sits deeper than {@link MAX_JSON_DEPTH}. Stops at
 * the first offender rather than measuring the whole body.
 *
 * Iterative, with its own explicit stack: a RECURSIVE depth check on a body
 * whose depth is the thing in question would overflow on exactly the input it
 * exists to reject.
 */
function exceedsMaxDepth(root: unknown): boolean {
  const stack: { value: unknown; depth: number }[] = [
    { value: root, depth: 1 },
  ];
  while (stack.length > 0) {
    const { value, depth } = stack.pop() as { value: unknown; depth: number };
    if (value === null || typeof value !== "object") continue;
    if (depth > MAX_JSON_DEPTH) return true;
    const children = Array.isArray(value)
      ? value
      : Object.values(value as Record<string, unknown>);
    for (const child of children)
      stack.push({ value: child, depth: depth + 1 });
  }
  return false;
}
