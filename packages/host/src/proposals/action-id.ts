/**
 * Model-supplied idempotency keys for write tools.
 *
 * A model that re-issues a write — because a run was retried, because it lost
 * track of what it already did, or because a correction pass re-ran the same
 * step — must not duplicate the effect. The key that makes that possible has to
 * come from the model (only it knows that "place the resistor" is the same
 * intent it expressed a moment ago), so the framework specifies a format,
 * validates it, and degrades gracefully when the model gets it wrong.
 */

/**
 * `<verb>_<primaryKey>_<scopeId>` — a lowercase verb, a key identifying the
 * thing being written, and the scope it belongs to.
 *
 * The shape is loose on purpose: it exists to catch a model emitting prose or a
 * uuid where an intent-stable key belongs, not to police naming. What the format
 * really guarantees is that the key mentions the *what* and the *where*, so two
 * different writes cannot collide on one id.
 */
export const ACTION_ID_PATTERN = /^[a-z]+_[^_]+.*_[A-Za-z0-9-]+$/;

/** True when `value` is a syntactically usable action id. */
export function isValidActionId(value: string): boolean {
  return ACTION_ID_PATTERN.test(value);
}

/**
 * Validate a model-supplied `action_id`.
 *
 * Returns the trimmed id when usable, or `undefined` — pushing an explanation
 * onto `warnings` — when it is malformed. A malformed key degrades the call to
 * non-idempotent; it never fails it. Rejecting the whole write over a formatting
 * slip would trade a small risk (a possible duplicate the dedup table cannot
 * catch) for a certain one (the work not happening at all), and the model gets
 * the warning back so it can fix the key on the next call.
 *
 * `raw` is typed `unknown` because it arrives from model-authored JSON: absent,
 * null, a number, and an object are all things a model has actually sent.
 */
export function normalizeActionId(
  raw: unknown,
  warnings: string[],
): string | undefined {
  if (raw === undefined || raw === null) return undefined;
  if (typeof raw !== "string") {
    warnings.push(
      `Ignored non-string action_id (${typeof raw}); expected "<verb>_<key>_<scopeId>".`,
    );
    return undefined;
  }
  const trimmed = raw.trim();
  if (trimmed.length === 0) return undefined;
  if (!isValidActionId(trimmed)) {
    warnings.push(
      `Ignored malformed action_id "${trimmed}" (expected "<verb>_<key>_<scopeId>"). This call is not idempotent.`,
    );
    return undefined;
  }
  return trimmed;
}

/**
 * Prompt text explaining the key to the model. Host-neutral: "scope" is whatever
 * the host's write tools operate on, and the host substitutes its own noun when
 * it composes the system prompt.
 */
export const ACTION_ID_GUIDANCE =
  "Every write tool takes an `action_id`: a stable idempotency key you choose, " +
  'formatted "<verb>_<primaryKey>_<scopeId>" (for example ' +
  "`create_item-42_scope-7`). Derive it from the intent, not from a random " +
  "value, so the same intent always produces the same key. Re-using an " +
  "action_id is a safe no-op: the write is recognised as one you already made " +
  "and is not repeated. If a write reports that an action_id already failed, do " +
  "not re-issue that key — inspect the current state and use a new action_id " +
  "for whatever still needs doing.";
