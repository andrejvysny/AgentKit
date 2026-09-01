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
   * Called at most once per distinct ref per provider pass; the caller caches
   * within a pass and never across one.
   */
  resolve(ref: string): Promise<ResolvedAttachment | null>;
}
