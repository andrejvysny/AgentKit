/**
 * Turning a stored attachment reference into bytes a provider can be shown.
 *
 * A message can carry an image part whose source is `{ kind: "ref", ref }`
 * (`AiImageSourceSchema` in `@agentkit/contracts`) — a handle into storage this
 * framework does not own and deliberately does not model. **The blob storage is
 * the host's**: a file on disk, a row, an S3 key, a content-addressed cache. The
 * ref is OPAQUE to everything here — AgentKit never parses it, never derives a
 * path from it, and never mints one.
 *
 * WHY A PORT AND NOT A COLUMN. The stored message keeps the ref forever, and the
 * bytes are fetched per provider pass, in memory, and thrown away. That is what
 * makes a conversation with a 4 MB screenshot in it cheap to append to, to fork,
 * to page through, and to replay — and it is what lets the same message be
 * re-resolved later at a different fidelity, or refused outright when the
 * attachment has been deleted since. Inlining the base64 into the record would
 * freeze one answer to all of those questions on the day the message was written.
 */

/** Bytes for one attachment, in the shape an image `data` source takes. */
export interface ResolvedAttachment {
  /** IANA media type of the payload, e.g. `"image/png"`. */
  mediaType: string;
  /** Raw base64 payload — no `data:` prefix, no media type. */
  base64: string;
}

/** Who is asking. Everything a resolver needs to scope the lookup. */
export interface AttachmentResolveContext {
  /** The conversation the referencing message belongs to. */
  chatId: string;
}

export interface AttachmentResolver {
  /**
   * The bytes behind a reference, or `null` when there are none to be had.
   *
   * `null` is a NORMAL answer, not an error path: an attachment can be deleted,
   * expired, garbage-collected, or belong to a workspace the current caller has
   * lost access to, and every one of those is a conversation that must still
   * run. The turn drops the image, warns `attachment_unresolved`, and answers
   * the question with the words it does have. THROW only for a genuine fault —
   * a storage backend that is down — where failing the turn is the honest
   * outcome.
   *
   * TREAT THIS AS AN AUTHORIZATION QUESTION, not a lookup. A ref arrives from an
   * untrusted client: it is whatever string a caller put in a message part, and
   * AgentKit neither mints nor parses it. So the question a resolver answers is
   * not "do these bytes exist" but "may THIS chat see them" — `null` means NOT
   * RESOLVABLE FOR THIS CHAT, which is also the honest answer for a ref that
   * belongs to another tenant, another user, or another workspace. A resolver
   * that ignores {@link ctx} and looks the ref up globally hands one chat's
   * attachments to anyone who can guess a ref.
   *
   * Called at most once per distinct ref per provider pass; the caller caches
   * within a pass and never across one.
   */
  resolve(
    ref: string,
    ctx: AttachmentResolveContext,
  ): Promise<ResolvedAttachment | null>;
}
