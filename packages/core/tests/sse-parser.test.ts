import { describe, expect, it } from "bun:test";
import { parseSseStream, type SseLine } from "../src/providers/sse.js";

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

  it("joins the several data: lines of one frame with a newline", async () => {
    const s = streamFromChunks(['data: {"a":\ndata: 1}\n\n']);
    expect(await collect(s)).toEqual(['{"a":\n1}']);
  });

  it("yields nothing for an event:-only frame", async () => {
    const s = streamFromChunks(["event: ping\n\ndata: a\n\n"]);
    const lines: SseLine[] = [];
    for await (const line of parseSseStream(s)) lines.push(line);
    expect(lines).toEqual([{ data: "a" }]);
  });

  it("reports [DONE] as a sentinel frame instead of swallowing it", async () => {
    const s = streamFromChunks(["data: x\n\n", "data: [DONE]\n\n"]);
    const lines: SseLine[] = [];
    for await (const line of parseSseStream(s)) lines.push(line);
    expect(lines).toEqual([{ data: "x" }, { data: "", done: true }]);
  });

  it("throws AbortError when the signal aborts mid-stream", async () => {
    const controller = new AbortController();
    // A body that delivers one frame and then simply stops: the read never
    // settles on its own, so only the abort can end this.
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        c.enqueue(new TextEncoder().encode("data: a\n\n"));
      },
    });
    const iterator = parseSseStream(stream, controller.signal);
    expect((await iterator.next()).value).toEqual({ data: "a" });
    const pending = iterator.next();
    controller.abort();
    await expect(pending).rejects.toMatchObject({ name: "AbortError" });
  });

  it("refuses to buffer a body with no frame boundary", async () => {
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        // 2 MiB of a single unterminated line.
        c.enqueue(new TextEncoder().encode("data: ".concat("x".repeat(2e6))));
        c.close();
      },
    });
    const run = async () => {
      for await (const _line of parseSseStream(stream)) {
        // drain
      }
    };
    await expect(run()).rejects.toThrow(/sse_parse/);
  });

  it("parses a chunk carrying far more than the cap in COMPLETE frames", async () => {
    // The cap used to be tested against the whole read, before any frame was
    // parsed out of it — so one big read from a fast provider (or a proxy that
    // coalesces) failed a perfectly healthy stream. What has to stay bounded is
    // the unparsed remainder, not the throughput.
    const frames = 30_000;
    const pad = "x".repeat(40);
    const chunk = Array.from(
      { length: frames },
      (_unused, i) => `data: ${i}-${pad}\n\n`,
    ).join("");
    // Well over the 1 MiB cap, in one read, and every byte of it well-formed.
    expect(chunk.length).toBeGreaterThan(1024 * 1024);
    const parsed = await collect(streamFromChunks([chunk]));
    expect(parsed).toHaveLength(frames);
    expect(parsed.at(-1)).toBe(`${frames - 1}-${pad}`);
  });
});
