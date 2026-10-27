import { mkdir, readdir, stat } from "node:fs/promises";
import { dirname } from "node:path";
import type { AiTool } from "@agentkit/core";
import { asRecord, optInt, optString, reqString } from "./args.js";
import { type PackEnv, defineTool } from "./context.js";
import { unifiedDiff } from "./diff.js";
import {
  type Edit,
  applyEditsToNormalizedContent,
  detectLineEnding,
  normalizeToLF,
  restoreLineEndings,
  splitBom,
} from "./edit-engine.js";
import { CodingError } from "./errors.js";
import {
  atomicWrite,
  looksBinary,
  readRegularFile,
  sha256,
} from "./fs-util.js";
import { withFileMutationQueue } from "./mutation-queue.js";
import { confine, isSensitiveSegment } from "./paths.js";
import { formatSize, splitLines, truncateHead } from "./truncate.js";

const NATIVE_HINT =
  "Prefer the native tools (read_file, list_dir, find_files, code_search, repo_map) over shell commands for exploring code.";

export function readFileTool(env: PackEnv): AiTool {
  return defineTool(
    {
      name: "read_file",
      effect: "read",
      capability: "fs.read",
      description: `Read a text file in the workspace. Output lines are numbered "<n>\\t<line>". Returns at most ${env.limits.readMaxLines} lines / ${formatSize(env.limits.readMaxBytes)} per call and ends with a continuation notice (offset=N) when more remains. offset is 1-based. ${NATIVE_HINT}`,
      inputSchema: {
        type: "object",
        properties: {
          path: {
            type: "string",
            description: "File path, relative to the workspace root.",
          },
          offset: {
            type: "integer",
            minimum: 1,
            description: "1-based first line to read.",
          },
          limit: {
            type: "integer",
            minimum: 1,
            description: "Maximum number of lines.",
          },
        },
        required: ["path"],
        additionalProperties: false,
      },
      timeoutMs: env.limits.toolTimeoutMs,
    },
    async (_ctx, raw) => {
      const input = asRecord(raw);
      const path = reqString(input, "path");
      const offset = optInt(input, "offset", 1, 1_000_000_000) ?? 1;
      const limit = optInt(input, "limit", 1, 1_000_000_000);
      const root = await env.root();
      const target = await confine(root, path, {
        access: "read",
        allowSensitive: env.allowSensitive,
      });
      const buffer = await readRegularFile(
        target.path,
        path,
        env.limits.readMaxFileBytes,
      );
      if (looksBinary(buffer))
        throw new CodingError(
          "binary_file",
          `${path} looks binary (${formatSize(buffer.length)}); not shown`,
        );
      const lines = splitLines(buffer.toString("utf-8"));
      if (lines.length === 0)
        return {
          ok: true,
          text: "(empty file)",
          summary: `read ${path} (empty)`,
        };
      if (offset > lines.length)
        throw new CodingError(
          "offset_out_of_range",
          `offset ${offset} is beyond the end of ${path} (${lines.length} lines)`,
        );
      const wanted = Math.min(
        limit ?? env.limits.readMaxLines,
        env.limits.readMaxLines,
      );
      const slice = lines.slice(offset - 1, offset - 1 + wanted);
      const numbered = slice
        .map((line, i) => `${offset + i}\t${line.replace(/\r$/, "")}`)
        .join("\n");
      const cut = truncateHead(numbered, {
        maxLines: slice.length,
        maxBytes: env.limits.readMaxBytes,
      });
      if (cut.firstLineExceedsLimit)
        throw new CodingError(
          "line_too_long",
          `line ${offset} of ${path} exceeds ${formatSize(env.limits.readMaxBytes)}; it cannot be shown`,
        );
      const last = offset + cut.outputLines - 1;
      const more = last < lines.length;
      const cappedByUs = cut.truncated || (limit === undefined && more);
      const notice = more
        ? `\n[Showing lines ${offset}-${last} of ${lines.length}${cut.truncatedBy === "bytes" ? ` (${formatSize(env.limits.readMaxBytes)} limit)` : ""}. Use offset=${last + 1} to continue.]`
        : "";
      return {
        ok: true,
        text: `${cut.content}${notice}`,
        summary: `read ${path} lines ${offset}-${last} of ${lines.length}`,
        truncated: cappedByUs,
      };
    },
  );
}

export function listDirTool(env: PackEnv): AiTool {
  return defineTool(
    {
      name: "list_dir",
      effect: "read",
      capability: "fs.read",
      description: `List a directory in the workspace (max ${env.limits.listMaxEntries} entries; directories end with /, symlinks with @; credential files are hidden). ${NATIVE_HINT}`,
      inputSchema: {
        type: "object",
        properties: {
          path: {
            type: "string",
            description:
              "Directory, relative to the workspace root. Default: the root.",
          },
        },
        additionalProperties: false,
      },
      timeoutMs: env.limits.toolTimeoutMs,
    },
    async (_ctx, raw) => {
      const input = asRecord(raw);
      const path = optString(input, "path") ?? ".";
      const root = await env.root();
      const target = await confine(root, path, {
        access: "read",
        allowSensitive: env.allowSensitive,
      });
      const info = await stat(target.path).catch(() => null);
      if (!info) throw new CodingError("not_found", `${path} does not exist`);
      if (!info.isDirectory())
        throw new CodingError("not_a_directory", `${path} is not a directory`);
      const all = await readdir(target.path, { withFileTypes: true });
      const visible = all
        .filter((e) => env.allowSensitive || !isSensitiveSegment(e.name))
        .sort((l, r) => (l.name < r.name ? -1 : l.name > r.name ? 1 : 0));
      const hidden = all.length - visible.length;
      const shown = visible.slice(0, env.limits.listMaxEntries);
      const lines = shown.map((e) =>
        e.isDirectory()
          ? `${e.name}/`
          : e.isSymbolicLink()
            ? `${e.name}@`
            : e.name,
      );
      const notes: string[] = [];
      if (visible.length > shown.length)
        notes.push(`[${visible.length - shown.length} more entries not shown]`);
      if (hidden > 0) notes.push(`[${hidden} sensitive entries hidden]`);
      return {
        ok: true,
        text: [...lines, ...notes].join("\n") || "(empty directory)",
        summary: `listed ${target.relativePath || "."} (${shown.length} entries)`,
        truncated: visible.length > shown.length,
      };
    },
  );
}

export function writeFileTool(env: PackEnv): AiTool {
  return defineTool(
    {
      name: "write_file",
      effect: "write",
      capability: "fs.write",
      description: `Create or overwrite a text file inside the workspace (parent directories are created; .git is refused; max ${formatSize(env.limits.writeMaxBytes)}). For changing part of an existing file use edit_file instead.`,
      inputSchema: {
        type: "object",
        properties: {
          path: {
            type: "string",
            description: "File path, relative to the workspace root.",
          },
          content: { type: "string", description: "Full new file content." },
        },
        required: ["path", "content"],
        additionalProperties: false,
      },
      timeoutMs: env.limits.toolTimeoutMs,
    },
    async (_ctx, raw) => {
      const input = asRecord(raw);
      const path = reqString(input, "path");
      const content = input["content"];
      if (typeof content !== "string")
        throw new CodingError(
          "invalid_arguments",
          '"content" must be a string',
        );
      const data = Buffer.from(content, "utf-8");
      if (data.length > env.limits.writeMaxBytes)
        throw new CodingError(
          "content_too_large",
          `content is ${data.length} bytes; the limit is ${env.limits.writeMaxBytes}`,
        );
      const root = await env.root();
      const options = {
        access: "write",
        allowSensitive: env.allowSensitive,
      } as const;
      const first = await confine(root, path, options);
      return withFileMutationQueue(first.path, async () => {
        const existed = (await stat(first.path).catch(() => null)) !== null;
        await mkdir(dirname(first.path), { recursive: true });
        // Re-resolve after mkdir: confirms no parent turned into an escape.
        const target = await confine(root, path, options);
        await atomicWrite(target.path, data, path);
        return {
          ok: true,
          text: `${existed ? "Overwrote" : "Created"} ${target.relativePath} (${data.length} bytes)\nsha256: ${sha256(data)}`,
          summary: `${existed ? "overwrote" : "created"} ${target.relativePath}`,
        };
      });
    },
  );
}

const MAX_EDITS = 50;

function parseEdits(value: unknown): Edit[] {
  if (!Array.isArray(value) || value.length === 0 || value.length > MAX_EDITS)
    throw new CodingError(
      "invalid_arguments",
      `"edits" must be an array of 1..${MAX_EDITS} {oldText,newText}`,
    );
  return value.map((item, i) => {
    const record = asRecord(item);
    const { oldText, newText } = record;
    if (typeof oldText !== "string" || typeof newText !== "string")
      throw new CodingError(
        "invalid_arguments",
        `edits[${i}] needs string oldText and newText`,
      );
    return { oldText, newText };
  });
}

export function editFileTool(env: PackEnv): AiTool {
  return defineTool(
    {
      name: "edit_file",
      effect: "write",
      capability: "fs.write",
      description:
        "Edit an existing text file with exact text replacement. Pass edits[]; each oldText must match exactly once in the ORIGINAL file (whitespace, quotes and trailing spaces are matched leniently only when no exact match exists) and edits must not overlap. Keep oldText as small as possible while unique. CRLF endings are preserved. Optional expectedSha256 refuses the edit if the file changed since you read it. Returns a unified diff and the new sha256.",
      inputSchema: {
        type: "object",
        properties: {
          path: {
            type: "string",
            description: "File path, relative to the workspace root.",
          },
          edits: {
            type: "array",
            minItems: 1,
            maxItems: MAX_EDITS,
            items: {
              type: "object",
              properties: {
                oldText: {
                  type: "string",
                  description: "Exact text to replace; unique in the file.",
                },
                newText: { type: "string", description: "Replacement text." },
              },
              required: ["oldText", "newText"],
              additionalProperties: false,
            },
          },
          expectedSha256: {
            type: "string",
            description: "sha256 (hex) the file must currently have.",
          },
        },
        required: ["path", "edits"],
        additionalProperties: false,
      },
      timeoutMs: env.limits.toolTimeoutMs,
    },
    async (_ctx, raw) => {
      const input = asRecord(raw);
      const path = reqString(input, "path");
      const edits = parseEdits(input["edits"]);
      const expected = optString(input, "expectedSha256", 64)?.toLowerCase();
      const root = await env.root();
      const options = {
        access: "write",
        allowSensitive: env.allowSensitive,
      } as const;
      const target = await confine(root, path, options);
      return withFileMutationQueue(target.path, async () => {
        const buffer = await readRegularFile(
          target.path,
          path,
          env.limits.editMaxFileBytes,
        );
        if (looksBinary(buffer))
          throw new CodingError(
            "binary_file",
            `${path} looks binary; not edited`,
          );
        const before = sha256(buffer);
        if (expected !== undefined && expected !== before)
          throw new CodingError(
            "sha256_mismatch",
            `${path} changed: expected sha256 ${expected}, current ${before}. Read it again.`,
          );
        const { bom, text } = splitBom(buffer.toString("utf-8"));
        const ending = detectLineEnding(text);
        const { baseContent, newContent } = applyEditsToNormalizedContent(
          normalizeToLF(text),
          edits,
          path,
        );
        const out = Buffer.from(
          bom + restoreLineEndings(newContent, ending),
          "utf-8",
        );
        const current = await confine(root, path, options);
        await atomicWrite(current.path, out, path);
        const after = sha256(out);
        const patch = unifiedDiff(target.relativePath, baseContent, newContent);
        const cut = truncateHead(patch.diff, {
          maxLines: Number.MAX_SAFE_INTEGER,
          maxBytes: env.limits.editMaxDiffBytes,
        });
        const diffText = cut.truncated
          ? `${cut.content}\n[diff truncated at ${formatSize(env.limits.editMaxDiffBytes)}]`
          : patch.diff;
        return {
          ok: true,
          text: `Edited ${target.relativePath} (${edits.length} edit${edits.length === 1 ? "" : "s"}, +${patch.added} -${patch.removed})\nsha256: ${after}\n${diffText}`,
          summary: `edited ${target.relativePath} (+${patch.added} -${patch.removed})`,
          truncated: cut.truncated,
          details: { path: target.relativePath, sha256: after, diff: diffText },
        };
      });
    },
  );
}
