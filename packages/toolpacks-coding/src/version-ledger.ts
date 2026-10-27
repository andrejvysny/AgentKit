/**
 * The version of each file a session last read or wrote, so a later mutation
 * can refuse to overwrite changes the session never saw.
 *
 * Module-global (like the coordinator) so every pack in the process, and every
 * run of one chat, shares one view. Bounded with LRU eviction; an evicted
 * entry just means "unknown" (write_file then asks for a re-read, edit_file
 * falls back to its under-lock content checks). Without a chatId nothing is
 * recorded.
 */
const MAX_PATHS_PER_CHAT = 4096;
const MAX_CHATS = 256;

/**
 * `full` is true when the session saw the WHOLE file at this version (a
 * complete read, or its own write). A partial read (offset, limit, or output
 * truncation) records `full: false`: it may anchor an edit_file, whose edits
 * are matched against oldText, but never authorises a full overwrite.
 */
export interface LedgerEntry {
  sha256: string;
  full: boolean;
}

const chats = new Map<string, Map<string, LedgerEntry>>();

function touchChat(chatId: string): Map<string, LedgerEntry> {
  const existing = chats.get(chatId);
  if (existing) {
    chats.delete(chatId);
    chats.set(chatId, existing);
    return existing;
  }
  const created = new Map<string, LedgerEntry>();
  chats.set(chatId, created);
  while (chats.size > MAX_CHATS) {
    const oldest = chats.keys().next();
    if (oldest.done) break;
    chats.delete(oldest.value);
  }
  return created;
}

function put(
  chatId: string | undefined,
  path: string,
  sha256: string,
  full: boolean,
): void {
  if (chatId === undefined) return;
  const paths = touchChat(chatId);
  const previous = paths.get(path);
  // A partial re-read of a version already seen in full keeps it full.
  const keepFull = previous?.full === true && previous.sha256 === sha256;
  paths.delete(path);
  paths.set(path, { sha256, full: full || keepFull });
  while (paths.size > MAX_PATHS_PER_CHAT) {
    const oldest = paths.keys().next();
    if (oldest.done) break;
    paths.delete(oldest.value);
  }
}

export function recordRead(
  chatId: string | undefined,
  canonicalPath: string,
  sha256: string,
  full: boolean,
): void {
  put(chatId, canonicalPath, sha256, full);
}

export function recordWrite(
  chatId: string | undefined,
  canonicalPath: string,
  sha256: string,
): void {
  put(chatId, canonicalPath, sha256, true);
}

export function expected(
  chatId: string | undefined,
  canonicalPath: string,
): LedgerEntry | undefined {
  if (chatId === undefined) return undefined;
  const paths = chats.get(chatId);
  const value = paths?.get(canonicalPath);
  if (value !== undefined && paths) {
    paths.delete(canonicalPath);
    paths.set(canonicalPath, value);
  }
  return value;
}

/** Test seam. */
export function clearVersionLedger(): void {
  chats.clear();
}
