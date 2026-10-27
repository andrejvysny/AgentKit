/**
 * Head and tail truncation of tool output by lines and bytes.
 *
 * Adapted from Pi (https://github.com/earendil-works/pi), v1.4.0, commit
 * 4a080252f06caf63a525d7873c5d94d3c23887e0, path
 * `packages/coding-agent/src/core/tools/truncate.ts`.
 * Copyright (c) 2025 Mario Zechner. MIT License.
 * Changes: no grep line helper, explicit limits, trimmed result shape.
 */

export interface Truncation {
  content: string;
  truncated: boolean;
  truncatedBy: "lines" | "bytes" | null;
  totalLines: number;
  totalBytes: number;
  outputLines: number;
  outputBytes: number;
  /** Tail only: the first kept line is the end of a longer line. */
  lastLinePartial: boolean;
  /** Head only: the very first line alone exceeds the byte limit. */
  firstLineExceedsLimit: boolean;
}

export interface TruncationLimits {
  maxLines: number;
  maxBytes: number;
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}

export function splitLines(content: string): string[] {
  if (content.length === 0) return [];
  const lines = content.split("\n");
  if (content.endsWith("\n")) lines.pop();
  return lines;
}

function untouched(content: string, lines: string[], bytes: number) {
  return {
    content,
    truncated: false,
    truncatedBy: null,
    totalLines: lines.length,
    totalBytes: bytes,
    outputLines: lines.length,
    outputBytes: bytes,
    lastLinePartial: false,
    firstLineExceedsLimit: false,
  } satisfies Truncation;
}

/** Keep the first lines/bytes; never returns a partial line. */
export function truncateHead(
  content: string,
  limits: TruncationLimits,
): Truncation {
  const totalBytes = Buffer.byteLength(content, "utf-8");
  const lines = splitLines(content);
  if (lines.length <= limits.maxLines && totalBytes <= limits.maxBytes)
    return untouched(content, lines, totalBytes);
  if (Buffer.byteLength(lines[0] ?? "", "utf-8") > limits.maxBytes)
    return {
      content: "",
      truncated: true,
      truncatedBy: "bytes",
      totalLines: lines.length,
      totalBytes,
      outputLines: 0,
      outputBytes: 0,
      lastLinePartial: false,
      firstLineExceedsLimit: true,
    };
  const kept: string[] = [];
  let bytes = 0;
  let truncatedBy: "lines" | "bytes" = "lines";
  for (let i = 0; i < lines.length && i < limits.maxLines; i++) {
    const line = lines[i] as string;
    const lineBytes = Buffer.byteLength(line, "utf-8") + (i > 0 ? 1 : 0);
    if (bytes + lineBytes > limits.maxBytes) {
      truncatedBy = "bytes";
      break;
    }
    kept.push(line);
    bytes += lineBytes;
  }
  const out = kept.join("\n");
  return {
    content: out,
    truncated: true,
    truncatedBy,
    totalLines: lines.length,
    totalBytes,
    outputLines: kept.length,
    outputBytes: Buffer.byteLength(out, "utf-8"),
    lastLinePartial: false,
    firstLineExceedsLimit: false,
  };
}

/** Keep the last lines/bytes (errors live at the end of command output). */
export function truncateTail(
  content: string,
  limits: TruncationLimits,
): Truncation {
  const totalBytes = Buffer.byteLength(content, "utf-8");
  const lines = splitLines(content);
  if (lines.length <= limits.maxLines && totalBytes <= limits.maxBytes)
    return untouched(content, lines, totalBytes);
  const kept: string[] = [];
  let bytes = 0;
  let truncatedBy: "lines" | "bytes" = "lines";
  let lastLinePartial = false;
  for (let i = lines.length - 1; i >= 0 && kept.length < limits.maxLines; i--) {
    const line = lines[i] as string;
    const lineBytes =
      Buffer.byteLength(line, "utf-8") + (kept.length > 0 ? 1 : 0);
    if (bytes + lineBytes > limits.maxBytes) {
      truncatedBy = "bytes";
      if (kept.length === 0) {
        const cut = tailBytes(line, limits.maxBytes);
        kept.unshift(cut);
        bytes = Buffer.byteLength(cut, "utf-8");
        lastLinePartial = true;
      }
      break;
    }
    kept.unshift(line);
    bytes += lineBytes;
  }
  const out = kept.join("\n");
  return {
    content: out,
    truncated: true,
    truncatedBy,
    totalLines: lines.length,
    totalBytes,
    outputLines: kept.length,
    outputBytes: Buffer.byteLength(out, "utf-8"),
    lastLinePartial,
    firstLineExceedsLimit: false,
  };
}

/** Last `maxBytes` of a string, starting on a UTF-8 character boundary. */
function tailBytes(value: string, maxBytes: number): string {
  const buf = Buffer.from(value, "utf-8");
  if (buf.length <= maxBytes) return value;
  let start = buf.length - maxBytes;
  while (start < buf.length && ((buf[start] as number) & 0xc0) === 0x80)
    start++;
  return buf.subarray(start).toString("utf-8");
}
