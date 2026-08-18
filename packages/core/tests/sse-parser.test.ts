import { describe, expect, it } from "bun:test";
import { parseSseStream } from "../src/providers/sse.js";

function streamFromChunks(chunks: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream({
    start(controller) {
      for (const c of chunks) controller.enqueue(encoder.encode(c));
      controller.close();
    },
  });
}

async function collect(stream: ReadableStream<Uint8Array>): Promise<string[]> {
  const out: string[] = [];
  for await (const line of parseSseStream(stream)) {
    if (line.data) out.push(line.data);
  }
  return out;
}

describe("parseSseStream", () => {
  it("parses single data lines", async () => {
    const s = streamFromChunks(["data: hello\n\n", "data: world\n\n"]);
    expect(await collect(s)).toEqual(["hello", "world"]);
  });

  it("handles fragmented chunks", async () => {
    const s = streamFromChunks(["data: hel", "lo\n", "\ndata: wo", "rld\n\n"]);
    expect(await collect(s)).toEqual(["hello", "world"]);
  });

  it("skips [DONE] sentinel", async () => {
    const s = streamFromChunks(["data: x\n\n", "data: [DONE]\n\n"]);
    expect(await collect(s)).toEqual(["x"]);
  });

  it("ignores comments", async () => {
    const s = streamFromChunks([": keep-alive\n\ndata: a\n\n"]);
    expect(await collect(s)).toEqual(["a"]);
  });

  it("handles CRLF line endings", async () => {
    const s = streamFromChunks(["data: hi\r\n\r\n"]);
    expect(await collect(s)).toEqual(["hi"]);
  });
});
