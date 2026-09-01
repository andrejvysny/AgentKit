/**
 * Turning an `Idempotency-Key` header into the task id a retry lands on.
 *
 * `TurnRunner.submitMessage` is already idempotent per caller-supplied
 * `taskId`: the second submit of one id writes nothing and returns the first
 * one's message ids. All this layer has to do is derive that id from the
 * header, and derive it the SAME way every time — including from another
 * process, after a restart, in a different replica. A random mapping held in
 * memory would make "the same key" mean "the same key on the same box since the
 * last deploy".
 *
 * The key is scoped by chat: two conversations may legitimately reuse a client's
 * counter, and hashing the pair keeps that from colliding into one turn. The
 * digest is SHA-256 over `<chatId>:<key>` via WebCrypto — a standard global in
 * Bun, Node ≥ 19 and browsers, so this package still imports nothing.
 */

/** Prefix so an id in a log line says where it came from. */
export const IDEMPOTENT_TASK_ID_PREFIX = "task_ik_";

/**
 * The same, for `regenerateMessage`.
 *
 * A DIFFERENT PREFIX is what keeps the two operations from colliding, and it is
 * the prefix rather than the hashed string that does the work. A regenerate
 * hashes `<chatId>:<messageId>:<key>` and a submit hashes `<chatId>:<key>`, so a
 * caller that submitted with the key `"<messageId>:<key>"` would otherwise hash
 * to the same digest as a regenerate of that message — and land on the run
 * belonging to a turn it never made. Domain-separating at the id, rather than
 * inside the hash, also leaves the submit derivation byte-identical to what it
 * has always been: changing it would strand every in-flight idempotency key
 * across a deploy.
 */
export const REGENERATE_TASK_ID_PREFIX = "task_rk_";

/**
 * The header `submitMessage` and `regenerateMessage` require; see
 * `REST_ROUTES`' note on the two.
 */
export const IDEMPOTENCY_KEY_HEADER = "idempotency-key";

/**
 * The deterministic task id for `(chatId, idempotencyKey)`.
 *
 * The pair is joined with `:` after the chat id, which cannot contain one in
 * any host this framework ships — and even if it could, the chat id is read
 * from the PATH while the key comes from a header, so a crafted key cannot
 * shift the boundary to impersonate another chat's turn: it would only ever
 * hash to a different id.
 */
export async function deriveIdempotentTaskId(
  chatId: string,
  idempotencyKey: string,
): Promise<string> {
  const bytes = new TextEncoder().encode(`${chatId}:${idempotencyKey}`);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return `${IDEMPOTENT_TASK_ID_PREFIX}${toHex(new Uint8Array(digest))}`;
}

/**
 * The deterministic task id for `(chatId, messageId, idempotencyKey)`.
 *
 * The message id is in the key because "regenerate" names a target: two
 * regenerates of two different answers are two different runs even under one
 * client-side key, and hashing only the chat would fold them into one and hand
 * the second caller the first one's branch.
 */
export async function deriveRegenerateTaskId(
  chatId: string,
  messageId: string,
  idempotencyKey: string,
): Promise<string> {
  const bytes = new TextEncoder().encode(
    `${chatId}:${messageId}:${idempotencyKey}`,
  );
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return `${REGENERATE_TASK_ID_PREFIX}${toHex(new Uint8Array(digest))}`;
}

function toHex(bytes: Uint8Array): string {
  let out = "";
  for (const byte of bytes) out += byte.toString(16).padStart(2, "0");
  return out;
}
