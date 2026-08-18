export interface SseLine {
  event?: string;
  data: string;
}

/**
 * Parse OpenAI-style `data: ...` SSE stream from a ReadableStream<Uint8Array>.
 * Yields one SseLine per `data:` payload (excluding the literal "[DONE]" sentinel).
 */
export async function* parseSseStream(
  stream: ReadableStream<Uint8Array>,
  signal?: AbortSignal,
): AsyncGenerator<SseLine, void, unknown> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    while (true) {
      if (signal?.aborted) {
        try {
          await reader.cancel();
        } catch {
          // ignore
        }
        return;
      }
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let idx: number;
      while ((idx = buffer.indexOf("\n")) !== -1) {
        const rawLine = buffer.slice(0, idx).replace(/\r$/, "");
        buffer = buffer.slice(idx + 1);
        if (rawLine.length === 0) continue;
        const parsed = parseLine(rawLine);
        if (parsed) yield parsed;
      }
    }
    if (buffer.trim().length > 0) {
      const parsed = parseLine(buffer.trim());
      if (parsed) yield parsed;
    }
  } finally {
    try {
      reader.releaseLock();
    } catch {
      // ignore
    }
  }
}

function parseLine(rawLine: string): SseLine | null {
  if (rawLine.startsWith(":")) return null; // comment
  if (rawLine.startsWith("data:")) {
    const data = rawLine.slice(5).trimStart();
    if (data === "[DONE]") return null;
    return { data };
  }
  if (rawLine.startsWith("event:")) {
    return { event: rawLine.slice(6).trim(), data: "" };
  }
  return null;
}
