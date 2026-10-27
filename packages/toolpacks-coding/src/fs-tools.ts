import { mkdir, readdir, stat } from "node:fs/promises";
import { dirname } from "node:path";
import type { AiTool } from "@agentkit/core";
import { asRecord, optBool, optInt, optString, reqString } from "./args.js";
import { type Outcome, type PackEnv, defineTool } from "./context.js";
import { unifiedDiff } from "./diff.js";
import { type Edit, applyEdits, splitBom } from "./edit-engine.js";
import { CodingError } from "./errors.js";
import {
  atomicWrite,
  decodeUtf8,
  looksBinary,
  readRegularFile,
  sha256,
} from "./fs-util.js";
import { withFileMutationQueue } from "./mutation-queue.js";
import { confine, isSensitiveSegment } from "./paths.js";
import { loadIgnoreMatcher } from "./search.js";
import { formatSize, splitLines, truncateHead } from "./truncate.js";

const NATIVE_HINT =
  "Prefer the native tools (read_file, list_dir, find_files, code_search, repo_map) over shell commands for exploring code.";

const MAX_READ_PATHS = 8;

interface FileRead {
  text: string;
  summary: string;
  truncated: boolean;
  sha256: string;
  totalLines: number;
  invalidUtf8: boolean;
}

async function readOne(
  env: PackEnv,
  root: string,
  path: string,
  offset: number,
  limit: number | undefined,
  maxBytes: number,
): Promise<FileRead> {
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
  const digest = sha256(buffer);
  const strict = decodeUtf8(buffer);
  const lines = splitLines(strict ?? buffer.toString("utf-8"));
  const common = {
    sha256: digest,
    totalLines: lines.length,
    invalidUtf8: strict === undefined,
  };
  if (lines.length === 0)
    return {
      ...common,
      text: "(empty file)",
      summary: `read ${path} (empty)`,
      truncated: false,
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
    maxBytes,
  });
  if (cut.firstLineExceedsLimit)
    throw new CodingError(
      "line_too_long",
      `line ${offset} of ${path} exceeds ${formatSize(maxBytes)}; it cannot be shown`,
    );
  const last = offset + cut.outputLines - 1;
  const more = last < lines.length;
  const notice = more
    ? `\n[Showing lines ${offset}-${last} of ${lines.length}${cut.truncatedBy === "bytes" ? ` (${formatSize(maxBytes)} limit)` : ""}. Use offset=${last + 1} to continue.]`
    : "";
  return {
    ...common,
    text: `${cut.content}${notice}`,
    summary: `read ${path} lines ${offset}-${last} of ${lines.length}`,
    truncated: cut.truncated || (limit === undefined && more),
  };
}

function readPaths(input: Record<string, unknown>): string[] {
  const single = input["path"];
  const many = input["paths"];
  if ((single === undefined) === (many === undefined))
    throw new CodingError(
      "invalid_arguments",
      'pass exactly one of "path" or "paths"',
    );
  if (many === undefined) return [reqString(input, "path")];
  if (
    !Array.isArray(many) ||
    many.length === 0 ||
    many.length > MAX_READ_PATHS ||
    many.some((p) => typeof p !== "string" || p.length === 0 || p.length > 4096)
  )
    throw new CodingError(
      "invalid_arguments",
      `"paths" must be 1..${MAX_READ_PATHS} non-empty strings`,
    );
  if (new Set(many).size !== many.length)
    throw new CodingError("invalid_arguments", '"paths" must be unique');
  return many as string[];
}

const INVALID_UTF8_WARNING =
  "not valid UTF-8; shown with replacement characters";

export function readFileTool(env: PackEnv): AiTool {
  return defineTool(
    {
      name: "read_file",
      effect: "read",
      capability: "fs.read",
      description: `Read text files in the workspace: one file with path, or up to ${MAX_READ_PATHS} with paths (each under a "==> path <==" header; the output cap is shared; a file that fails is reported inline). Output lines are numbered "<n>\\t<line>". Returns at most ${env.limits.readMaxLines} lines per file and ${formatSize(env.limits.readMaxBytes)} in total, and ends with a continuation notice (offset=N) when more remains. offset is 1-based. The result data carries sha256 (of the whole file) and totalLines: pass that sha256 as edit_file's expectedSha256. ${NATIVE_HINT}`,
      inputSchema: {
        type: "object",
        properties: {
          path: {
            type: "string",
            description: "File path, relative to the workspace root.",
          },
          paths: {
            type: "array",
            items: { type: "string" },
            minItems: 1,
            maxItems: MAX_READ_PATHS,
            description: `Several files (1..${MAX_READ_PATHS}) instead of path.`,
          },
          offset: {
            type: "integer",
            minimum: 1,
            description: "1-based first line to read (applies to every file).",
          },
          limit: {
            type: "integer",
            minimum: 1,
            description: "Maximum number of lines per file.",
          },
        },
        oneOf: [{ required: ["path"] }, { required: ["paths"] }],
        additionalProperties: false,
      },
      timeoutMs: env.limits.toolTimeoutMs,
    },
    async (ctx, raw) => {
      const input = asRecord(raw);
      const paths = readPaths(input);
      const offset = optInt(input, "offset", 1, 1_000_000_000) ?? 1;
      const limit = optInt(input, "limit", 1, 1_000_000_000);
      const root = await env.root();
      if (input["path"] !== undefined) {
        ctx.signal?.throwIfAborted();
        const one = await readOne(
          env,
          root,
          paths[0] as string,
          offset,
          limit,
          env.limits.readMaxBytes,
        );
        return {
          ok: true,
          text: one.text,
          summary: one.summary,
          truncated: one.truncated,
          ...(one.invalidUtf8 ? { warnings: [INVALID_UTF8_WARNING] } : {}),
          details: {
            path: paths[0],
            sha256: one.sha256,
            totalLines: one.totalLines,
            ...(one.invalidUtf8 ? { encoding: "invalid-utf8" } : {}),
          },
        };
      }
      const share = Math.floor(env.limits.readMaxBytes / paths.length);
      const blocks: string[] = [];
      const files: Array<Record<string, unknown>> = [];
      const warnings: string[] = [];
      let failed = 0;
      let truncated = false;
      for (const path of paths) {
        ctx.signal?.throwIfAborted();
        try {
          const one = await readOne(env, root, path, offset, limit, share);
          blocks.push(`==> ${path} <==\n${one.text}`);
          truncated ||= one.truncated;
          if (one.invalidUtf8)
            warnings.push(`${path}: ${INVALID_UTF8_WARNING}`);
          files.push({
            path,
            sha256: one.sha256,
            totalLines: one.totalLines,
            ...(one.invalidUtf8 ? { encoding: "invalid-utf8" } : {}),
          });
        } catch (error) {
          if (!(error instanceof CodingError)) throw error;
          failed++;
          blocks.push(`==> ${path} <==\n${error.code}: ${error.message}`);
          files.push({ path, error: error.code });
        }
      }
      const outcome: Outcome = {
        ok: failed < paths.length,
        text: blocks.join("\n\n"),
        summary: `read ${paths.length - failed} of ${paths.length} files`,
        truncated,
        details: { files },
      };
      if (warnings.length > 0) outcome.warnings = warnings;
      return outcome;
    },
  );
}

export function listDirTool(env: PackEnv): AiTool {
  return defineTool(
    {
      name: "list_dir",
      effect: "read",
      capability: "fs.read",
      description: `List a directory in the workspace (max ${env.limits.listMaxEntries} entries; directories end with /, symlinks with @; credential files and entries matched by .gitignore are hidden unless includeIgnored is set). ${NATIVE_HINT}`,
      inputSchema: {
        type: "object",
        properties: {
          path: {
            type: "string",
            description:
              "Directory, relative to the workspace root. Default: the root.",
          },
          includeIgnored: {
            type: "boolean",
            description:
              "Also list entries matched by .gitignore / .ignore / .git/info/exclude.",
          },
        },
        additionalProperties: false,
      },
      timeoutMs: env.limits.toolTimeoutMs,
    },
    async (ctx, raw) => {
      const input = asRecord(raw);
      const path = optString(input, "path") ?? ".";
      const includeIgnored = optBool(input, "includeIgnored") ?? false;
      ctx.signal?.throwIfAborted();
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
      ctx.signal?.throwIfAborted();
      const isIgnored = includeIgnored
        ? () => false
        : await loadIgnoreMatcher(target.path, root, env.limits);
      const permitted = all.filter(
        (e) => env.allowSensitive || !isSensitiveSegment(e.name),
      );
      const visible = permitted
        .filter((e) => !isIgnored(e.name, e.isDirectory()))
        .sort((l, r) => (l.name < r.name ? -1 : l.name > r.name ? 1 : 0));
      const hidden = all.length - permitted.length;
      const ignored = permitted.length - visible.length;
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
      if (ignored > 0) notes.push(`[${ignored} ignored entries hidden]`);
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
    async (ctx, raw) => {
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
        ctx.signal?.throwIfAborted();
        await mkdir(dirname(first.path), { recursive: true });
        // Re-resolve after mkdir: confirms no parent turned into an escape.
        const target = await confine(root, path, options);
        ctx.signal?.throwIfAborted();
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
        "Edit an existing text file with exact text replacement. Pass edits[]; each oldText must match exactly once in the ORIGINAL file (whitespace, quotes and trailing spaces are matched leniently only when no exact match exists) and edits must not overlap. Keep oldText as small as possible while unique. Each line's original terminator (CRLF, LF or CR) and any BOM are preserved; new lines use the file's dominant ending. Files that are not valid UTF-8 are refused. Optional expectedSha256 refuses the edit if the file changed since you read it. Returns a unified diff and the new sha256.",
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
    async (ctx, raw) => {
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
        ctx.signal?.throwIfAborted();
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
        const decoded = decodeUtf8(buffer);
        if (decoded === undefined)
          throw new CodingError(
            "encoding_unsupported",
            `${path} is not valid UTF-8; not edited (editing would corrupt it)`,
          );
        const { bom, text } = splitBom(decoded);
        const { baseContent, newContent, output } = applyEdits(
          text,
          edits,
          path,
        );
        const out = Buffer.from(bom + output, "utf-8");
        const current = await confine(root, path, options);
        ctx.signal?.throwIfAborted();
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
