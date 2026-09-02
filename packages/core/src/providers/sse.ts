export interface SseLine {
  event?: string;
  data: string;
  /**
   * True for the `[DONE]` sentinel frame (`data` is empty). Yielded rather than
   * swallowed so a caller can tell "the provider said it was finished" from
   * "the socket stopped producing bytes" — the two look identical otherwise.
   */
  done?: boolean;
}

/**
 * Hard cap on unparsed bytes held in memory. A server that never sends a
 * newline (or a proxy streaming a non-SSE body) would otherwise grow `buffer`
 * until the process dies; failing loudly at 1 MiB is the bounded outcome.
 */
const MAX_BUFFER_CHARS = 1024 * 1024;

/**
 * Parse an SSE stream from a `ReadableStream<Uint8Array>` into one
 * {@link SseLine} per *frame* (blank-line terminated), per the EventSource
 * spec: several `data:` lines in one frame are joined with `\n`, comments are
 * skipped, and a frame with no `data:` line at all (an `event:`-only heartbeat)
 * yields nothing.
 *
 * Cancellation: an aborted `signal` cancels the reader and THROWS an
 * `AbortError` rather than returning cleanly. A clean return is indistinguishable
 * from "the provider finished", which is how a cancelled run used to be
 * committed as a completed half-sentence. The `finally` always cancels the
 * reader too — releasing the lock alone leaves the underlying socket open.
 */
export async function* parseSseStream(
  stream: ReadableStream<Uint8Array>,
  signal?: AbortSignal,
): AsyncGenerator<SseLine, void, unknown> {
  const reader: StreamReader = stream.getReader();
  const decoder = new TextDecoder();
  const frame: FrameState = { data: [] };
  let buffer = "";
  try {
    while (true) {
      const { value, done } = await readOrAbort(reader, signal);
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let idx: number;
      // biome-ignore lint/suspicious/noAssignInExpressions: the assign-and-test scan loop is the canonical incremental-parser idiom; splitting it duplicates the indexOf call
      while ((idx = buffer.indexOf("\n")) !== -1) {
        const rawLine = buffer.slice(0, idx).replace(/\r$/, "");
        buffer = buffer.slice(idx + 1);
        if (rawLine.length === 0) {
          const line = flushFrame(frame);
          if (line) yield line;
          continue;
        }
        appendField(frame, rawLine);
      }
      // Checked on what is LEFT after every complete frame has been parsed out,
      // never on the whole read: one chunk carrying several MiB of well-formed
      // frames is a fast provider, not an attack, and failing it killed healthy
      // streams. What must stay bounded is the unterminated remainder — the
      // server that never sends a newline.
      if (buffer.length > MAX_BUFFER_CHARS) {
        throw new Error(
          `sse_parse: no frame boundary within ${MAX_BUFFER_CHARS} bytes; refusing to buffer more.`,
        );
      }
    }
    // A stream that ends without its final blank line still owes us the frame
    // it had already fully described.
    if (buffer.length > 0) appendField(frame, buffer.trimEnd());
    const last = flushFrame(frame);
    if (last) yield last;
  } finally {
    try {
      await reader.cancel();
    } catch {
      // ignore
    }
    try {
      reader.releaseLock();
    } catch {
      // ignore
    }
  }
}

/**
 * Just enough of a stream reader for this parser, described structurally: the
 * DOM and `node:stream/web` declarations of `ReadableStreamDefaultReader` are
 * not mutually assignable, and both reach this file depending on the consumer.
 */
interface StreamReader {
  read(): Promise<{ done: boolean; value?: Uint8Array }>;
  cancel(): Promise<void>;
  releaseLock(): void;
}

/**
 * `reader.read()`, but an abort wins the race and throws.
 *
 * A real aborted `fetch` rejects its pending read; a stalled proxy — or any
 * body that simply stops producing chunks — does not, and waiting on it would
 * park a cancelled run forever. The abandoned read keeps a no-op handler so a
 * late rejection is never an unhandled one.
 */
async function readOrAbort(
  reader: StreamReader,
  signal: AbortSignal | undefined,
): Promise<Awaited<ReturnType<StreamReader["read"]>>> {
  if (!signal) return reader.read();
  if (signal.aborted) throw abortError();
  const read = reader.read();
  read.catch(() => {});
  let onAbort: (() => void) | undefined;
  try {
    return await Promise.race([
      read,
      new Promise<never>((_resolve, reject) => {
        onAbort = () => reject(abortError());
        signal.addEventListener("abort", onAbort, { once: true });
      }),
    ]);
  } finally {
    if (onAbort) signal.removeEventListener("abort", onAbort);
  }
}

/** Fields of the frame being accumulated, reset at every blank line. */
interface FrameState {
  data: string[];
  event?: string;
}

/** Apply one non-blank SSE line to the in-progress frame. */
function appendField(frame: FrameState, rawLine: string): void {
  if (rawLine.startsWith(":")) return; // comment
  if (rawLine.startsWith("data:")) {
    // The spec strips exactly one leading space, not all whitespace: the rest
    // is payload.
    frame.data.push(rawLine.slice(5).replace(/^ /, ""));
    return;
  }
  if (rawLine.startsWith("event:")) {
    frame.event = rawLine.slice(6).trim();
    return;
  }
  // `id:` / `retry:` / unknown fields: no meaning to this parser.
}

/** Emit the accumulated frame (if it carried data) and reset the state. */
function flushFrame(frame: FrameState): SseLine | null {
  const data = frame.data.join("\n");
  const hadData = frame.data.length > 0;
  const event = frame.event;
  frame.data = [];
  frame.event = undefined;
  if (!hadData) return null;
  if (data === "[DONE]") return { data: "", done: true };
  return event === undefined ? { data } : { event, data };
}

/** An `AbortError` without depending on `DOMException` (works in every realm). */
function abortError(): Error {
  const err = new Error("SSE stream aborted");
  err.name = "AbortError";
  return err;
}
