/**
 * A Server-Sent Events parser over a `ReadableStream`, because `EventSource`
 * cannot carry an `Authorization` header.
 *
 * That is the whole reason this file exists. `EventSource` is the browser's own
 * SSE client and it does resume, backoff and framing for free — but its only
 * configuration is `withCredentials`, so an API secured by a bearer token (which
 * is every deployment this client is written for) cannot use it. `fetch` can
 * send the header; what it cannot do is frame the bytes, so the framing is here.
 *
 * The grammar implemented is the WHATWG one, narrowed to what the server writes
 * (`packages/transport-http/src/sse.ts`): `id:`, `event:`, `data:`, `retry:`,
 * `:` comments for heartbeats, and a blank line to dispatch. Multi-line `data`
 * is joined with `\n` even though this server never emits it — a parser that
 * only handles the frames it has seen is a parser that corrupts the first one it
 * has not.
 */

/** One dispatched frame. `data` is absent for a comment-only or `retry:` frame. */
export interface SseFrame {
  id?: string;
  event?: string;
  data?: string;
  /** The server's reconnection hint, in milliseconds. */
  retry?: number;
}

/**
 * Frames, in order, until the stream ends.
 *
 * A stream that ends mid-frame yields nothing for the partial: an incomplete
 * frame is an incomplete JSON body, and half an event is worse than a missing
 * one — the resume that follows starts from the last COMPLETE event's id, so the
 * partial arrives again whole.
 */
export async function* parseSseStream(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<SseFrame> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let frame: SseFrame = {};
  let data: string[] = [];
  let sawField = false;

  const dispatch = (): SseFrame | null => {
    const out = data.length === 0 ? frame : { ...frame, data: data.join("\n") };
    const dispatched = sawField ? out : null;
    frame = {};
    data = [];
    sawField = false;
    return dispatched;
  };

  try {
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) break;
      buffer += decoder.decode(chunk.value, { stream: true });

      // Line terminators per the spec: CRLF, LF or a lone CR.
      for (;;) {
        const match = /\r\n|\n|\r/.exec(buffer);
        if (match === null) break;
        const line = buffer.slice(0, match.index);
        buffer = buffer.slice(match.index + match[0].length);

        if (line === "") {
          const dispatched = dispatch();
          if (dispatched !== null) yield dispatched;
          continue;
        }
        // A comment — the server's `: hb` keepalive. Proof of life, no more.
        if (line.startsWith(":")) continue;

        const colon = line.indexOf(":");
        const field = colon === -1 ? line : line.slice(0, colon);
        // Exactly ONE leading space is stripped, per the spec: `data:  x` is
        // the value " x", and trimming would silently rewrite a payload.
        let value = colon === -1 ? "" : line.slice(colon + 1);
        if (value.startsWith(" ")) value = value.slice(1);

        sawField = true;
        switch (field) {
          case "id":
            // A NUL in the id is ignored rather than stored — the spec's rule,
            // and the id is about to become a `Last-Event-ID` request header.
            if (!value.includes("\0")) frame.id = value;
            break;
          case "event":
            frame.event = value;
            break;
          case "data":
            data.push(value);
            break;
          case "retry": {
            if (/^\d+$/.test(value)) frame.retry = Number(value);
            break;
          }
          default:
            // Unknown field: ignored, so a later contract can add one without
            // breaking this parser.
            break;
        }
      }
    }
  } finally {
    // Cancels the underlying body when the consumer stops early — a `break` out
    // of `for await`, an abort, or a throw. Without it the response body stays
    // open and the server keeps polling for a reader that has gone.
    await reader.cancel().catch(() => undefined);
    try {
      reader.releaseLock();
    } catch {
      // A pending read makes this throw; the cancel above has already ended the
      // stream, and throwing here would replace the real error with this one.
    }
  }
}
