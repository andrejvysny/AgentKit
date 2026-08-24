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
): Promise<{ ok: true; value: Record<string, unknown> } | { ok: false; response: Response }> {
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
): { ok: true; value?: number } | { ok: false; message: string } {
  const raw = url.searchParams.get(name);
  if (raw === null) return { ok: true };
  const value = Number(raw);
  if (!Number.isInteger(value) || value <= 0) {
    return { ok: false, message: `Query parameter \`${name}\` must be a positive integer.` };
  }
  return { ok: true, value };
}
