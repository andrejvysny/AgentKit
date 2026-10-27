import { readdir, stat } from "node:fs/promises";
import { join } from "node:path";
import type { AiTool } from "@agentkit/core";
import { asRecord, optBool, optInt, optString, reqString } from "./args.js";
import { type PackEnv, defineTool } from "./context.js";
import { CodingError } from "./errors.js";
import { readRegularFile } from "./fs-util.js";
import { confine } from "./paths.js";
import {
  type SearchBudget,
  type WalkOptions,
  findFiles,
  searchText,
  walkTree,
} from "./search.js";

const PREFER_NATIVE =
  "Use this instead of grep, rg, find or ls in a shell command.";

function walkBase(
  env: PackEnv,
  signal: AbortSignal | undefined,
): Omit<WalkOptions, "budget" | "inheritedFrom"> {
  return {
    limits: env.limits,
    allowSensitive: env.allowSensitive,
    clock: env.clock,
    signal,
  };
}

export function findFilesTool(env: PackEnv): AiTool {
  return defineTool(
    {
      name: "find_files",
      effect: "read",
      capability: "fs.read",
      description: `Find files by glob (e.g. **/*.ts, src/**/*.test.{ts,tsx}). Respects .gitignore, .ignore and .git/info/exclude; skips .git, node_modules, symlinks and credential files. Up to ${env.limits.findMaxResults} results. ${PREFER_NATIVE}`,
      inputSchema: {
        type: "object",
        properties: {
          pattern: {
            type: "string",
            description:
              "Glob matched against the path relative to `path`, or the file name.",
          },
          path: {
            type: "string",
            description: "Directory to search. Default: the workspace root.",
          },
          limit: {
            type: "integer",
            minimum: 1,
            maximum: env.limits.findMaxResults,
          },
        },
        required: ["pattern"],
        additionalProperties: false,
      },
      timeoutMs: env.limits.searchTimeBudgetMs + 5000,
    },
    async (ctx, raw) => {
      const input = asRecord(raw);
      const pattern = reqString(
        input,
        "pattern",
        env.limits.searchMaxPatternChars,
      );
      const path = optString(input, "path") ?? ".";
      const limit =
        optInt(input, "limit", 1, env.limits.findMaxResults) ??
        env.limits.findDefaultResults;
      const root = await env.root();
      const target = await confine(root, path, {
        access: "read",
        allowSensitive: env.allowSensitive,
      });
      const info = await stat(target.path).catch(() => null);
      if (!info?.isDirectory())
        throw new CodingError("not_a_directory", `${path} is not a directory`);
      const found = await findFiles(
        pattern,
        target.path,
        root,
        limit,
        walkBase(env, ctx.signal),
      );
      const body = found.paths.join("\n") || "No files matched";
      return {
        ok: true,
        text: found.stopped ? `${body}\n[stopped: ${found.stopped}]` : body,
        summary: `${found.paths.length} file(s) matched ${pattern}`,
        truncated: found.stopped !== "",
      };
    },
  );
}

export function codeSearchTool(env: PackEnv): AiTool {
  const l = env.limits;
  return defineTool(
    {
      name: "code_search",
      effect: "read",
      capability: "fs.read",
      description: `Search text files for a regex or literal. Output lines are "<path>:<line>:<text>". Respects .gitignore, .ignore and .git/info/exclude; skips .git, node_modules, symlinks, binary and credential files. Bounded: ${l.searchMaxFiles} entries, ${l.searchMaxFileBytes / 1024 / 1024} MiB per file, ${l.searchMaxOutputBytes / 1024} KiB output, ${l.searchTimeBudgetMs / 1000}s; a bounded search says so. ${PREFER_NATIVE}`,
      inputSchema: {
        type: "object",
        properties: {
          pattern: {
            type: "string",
            description:
              "Regular expression (or literal text with literal=true).",
          },
          path: {
            type: "string",
            description: "File or directory. Default: the workspace root.",
          },
          glob: {
            type: "string",
            description: "Only files whose path or name matches, e.g. *.ts.",
          },
          literal: {
            type: "boolean",
            description: "Treat pattern as plain text.",
          },
          ignoreCase: { type: "boolean" },
          context: {
            type: "integer",
            minimum: 0,
            maximum: l.searchMaxContext,
            description: "Lines of context around each match.",
          },
          limit: {
            type: "integer",
            minimum: 1,
            maximum: l.searchMaxMatches,
            description: `Maximum matches (default ${l.searchDefaultMatches}).`,
          },
          offset: {
            type: "integer",
            minimum: 0,
            description:
              "Matches to skip, to page through a truncated result (use the offset the notice gives).",
          },
        },
        required: ["pattern"],
        additionalProperties: false,
      },
      timeoutMs: l.searchTimeBudgetMs + 5000,
    },
    async (ctx, raw) => {
      const input = asRecord(raw);
      const pattern = reqString(input, "pattern", l.searchMaxPatternChars);
      const path = optString(input, "path") ?? ".";
      const root = await env.root();
      const target = await confine(root, path, {
        access: "read",
        allowSensitive: env.allowSensitive,
      });
      const outcome = await searchText(
        {
          target: target.path,
          workspaceRoot: root,
          pattern,
          glob: optString(input, "glob", 1000),
          literal: optBool(input, "literal") ?? false,
          ignoreCase: optBool(input, "ignoreCase") ?? false,
          context: optInt(input, "context", 0, l.searchMaxContext) ?? 0,
          limit:
            optInt(input, "limit", 1, l.searchMaxMatches) ??
            l.searchDefaultMatches,
          offset: optInt(input, "offset", 0, 1_000_000_000) ?? 0,
        },
        walkBase(env, ctx.signal),
      );
      return {
        ok: true,
        text: outcome.text,
        summary: `${outcome.matches} match(es) for ${pattern.slice(0, 60)}`,
        truncated: outcome.truncated,
      };
    },
  );
}

const MANIFESTS = [
  "package.json",
  "tsconfig.json",
  "cargo.toml",
  "pyproject.toml",
  "go.mod",
  "readme.md",
  "readme",
];

export function repoMapTool(env: PackEnv): AiTool {
  const l = env.limits;
  return defineTool(
    {
      name: "repo_map",
      effect: "read",
      capability: "fs.read",
      description: `Orient yourself in a repository: top-level manifests (package.json, tsconfig, Cargo.toml, pyproject, go.mod, README) with short excerpts, plus a depth-limited directory tree (default depth ${l.repoMapDefaultDepth}, max ${l.repoMapMaxEntries} entries). Respects ignore files. Call this first, before any shell command.`,
      inputSchema: {
        type: "object",
        properties: {
          path: {
            type: "string",
            description: "Directory. Default: the workspace root.",
          },
          depth: { type: "integer", minimum: 1, maximum: l.repoMapMaxDepth },
        },
        additionalProperties: false,
      },
      timeoutMs: l.searchTimeBudgetMs + 5000,
    },
    async (ctx, raw) => {
      const input = asRecord(raw);
      const path = optString(input, "path") ?? ".";
      const depth =
        optInt(input, "depth", 1, l.repoMapMaxDepth) ?? l.repoMapDefaultDepth;
      const root = await env.root();
      const target = await confine(root, path, {
        access: "read",
        allowSensitive: env.allowSensitive,
      });
      const info = await stat(target.path).catch(() => null);
      if (!info?.isDirectory())
        throw new CodingError("not_a_directory", `${path} is not a directory`);
      const sections: string[] = [];
      // Match against the real directory entries: a case-insensitive
      // filesystem would otherwise show README.md and readme.md twice.
      const present = (await readdir(target.path).catch(() => [])).sort();
      for (const wanted of MANIFESTS) {
        const name = present.find((entry) => entry.toLowerCase() === wanted);
        if (name === undefined) continue;
        const text = await readExcerpt(
          join(target.path, name),
          name,
          l.repoMapManifestExcerptBytes,
        );
        if (text !== undefined) sections.push(`== ${name} ==\n${text}`);
      }
      const budget: SearchBudget = {};
      const entries: Array<{ rel: string; isDirectory: boolean }> = [];
      let more = false;
      for await (const entry of walkTree(target.path, {
        ...walkBase(env, ctx.signal),
        budget,
        inheritedFrom: root,
        includeDirectories: true,
        maxDepth: depth,
      })) {
        if (entries.length >= l.repoMapMaxEntries) {
          more = true;
          break;
        }
        entries.push({ rel: entry.rel, isDirectory: entry.isDirectory });
      }
      const key = (rel: string) => rel.split("/");
      entries.sort((a, b) => compareSegments(key(a.rel), key(b.rel)));
      const tree = entries.map((e) => {
        const parts = e.rel.split("/");
        return `${"  ".repeat(parts.length - 1)}${parts[parts.length - 1]}${e.isDirectory ? "/" : ""}`;
      });
      const notes = [
        more
          ? `${l.repoMapMaxEntries} entries shown; raise depth or narrow path`
          : "",
        budget.entriesExhausted ? `${l.searchMaxFiles} entries scanned` : "",
        budget.timedOut
          ? `${l.searchTimeBudgetMs / 1000}s of scanning elapsed`
          : "",
      ].filter(Boolean);
      sections.push(
        `== tree (depth ${depth}) ==\n${tree.join("\n") || "(empty)"}`,
      );
      if (notes.length) sections.push(`[stopped: ${notes.join("; ")}]`);
      return {
        ok: true,
        text: sections.join("\n\n"),
        summary: `mapped ${target.relativePath || "."} (${entries.length} entries)`,
        truncated: notes.length > 0,
      };
    },
  );
}

function compareSegments(a: string[], b: string[]): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    const x = a[i] as string;
    const y = b[i] as string;
    if (x !== y) return x < y ? -1 : 1;
  }
  return a.length - b.length;
}

async function readExcerpt(
  path: string,
  label: string,
  maxBytes: number,
): Promise<string | undefined> {
  try {
    const buffer = await readRegularFile(path, label, 1024 * 1024);
    if (buffer.subarray(0, 4096).includes(0)) return undefined;
    const text = buffer.subarray(0, maxBytes).toString("utf-8");
    return buffer.length > maxBytes
      ? `${text}\n[... ${buffer.length - maxBytes} more bytes]`
      : text;
  } catch {
    return undefined;
  }
}
