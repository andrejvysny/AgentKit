/**
 * The opaque page cursor `MessagePageDto.nextCursor` carries.
 *
 * It encodes the store's `orderKey`, which the message DTO deliberately omits —
 * so the cursor is the only handle a client has on position, and it must stay
 * unreadable enough that nobody builds on its contents. Base-36 behind a letter
 * is not encryption and is not meant to be; it is a shape that no client will
 * mistake for an integer it can add one to.
 */

const CURSOR_PREFIX = "m";

export function encodeMessageCursor(orderKey: number): string {
  return `${CURSOR_PREFIX}${orderKey.toString(36)}`;
}

/** The `orderKey` a cursor names, or null when it is not one we wrote. */
export function decodeMessageCursor(cursor: string): number | null {
  if (!cursor.startsWith(CURSOR_PREFIX)) return null;
  const value = Number.parseInt(cursor.slice(CURSOR_PREFIX.length), 36);
  if (!Number.isInteger(value) || value < 0) return null;
  return value;
}
