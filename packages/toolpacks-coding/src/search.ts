/**
 * Bounded, ignore-aware filesystem walk and text search in pure TypeScript.
 *
 * Adapted from OneMind (https://github.com/andrejvysny/OneMind), commit
 * 51abbbd, path `src-ts/src/code/pi-worker/search.ts`.
 * Copyright (c) 2026 OneMind contributors. MIT License.
 * Changes: no Bun.Glob (own glob compiler with braces), no Pi grep wrapper,
 * limits come from `CodingLimits`, directories can be yielded, depth bound,
 * injectable clock, `.git`/`node_modules`/symlinks/sensitive names skipped.
 */
import { readFile, readdir, stat } from "node:fs/promises";
import { basename, join, relative, sep } from "node:path";
import { CodingError } from "./errors.js";
import type { CodingLimits } from "./limits.js";
import { isSensitiveSegment } from "./paths.js";

/** Git's exclude files plus the ripgrep-style `.ignore`. */
const IGNORE_FILES = [".gitignore", ".ignore", join(".git", "info", "exclude")];

/** Why a walk ended early; reported to the model instead of failing. */
export interface SearchBudget {
  entriesExhausted?: boolean;
  timedOut?: boolean;
}

interface IgnoreRule {
  base: string;
  pattern: RegExp;
  negate: boolean;
  directoryOnly: boolean;
}

export interface WalkEntry {
  path: string;
  /** Relative to the walk root, with `/` separators. */
  rel: string;
  isDirectory: boolean;
  depth: number;
}

export interface WalkOptions {
  limits: CodingLimits;
  allowSensitive: boolean;
  clock: () => number;
  budget: SearchBudget;
  signal?: AbortSignal | undefined;
  /** Workspace root: ignore files above the walk root still apply. */
  inheritedFrom?: string | undefined;
  includeDirectories?: boolean;
  /** Entries deeper than this are not listed (root children are depth 1). */
  maxDepth?: number;
}

function aborted(): never {
  throw new CodingError("aborted", "the call was aborted");
}

/**
 * Shallow-first walk in name order: a search that must stop early has covered
 * the top of the project. Never enters `.git`, `node_modules`, symlinks,
 * ignored paths or (unless allowed) sensitive names.
 */
export async function* walkTree(
  root: string,
  options: WalkOptions,
): AsyncGenerator<WalkEntry> {
  const cache = new Map<string, IgnoreRule[]>();
  const queue: Array<{
    directory: string;
    rules: IgnoreRule[];
    depth: number;
  }> = [
    {
      directory: root,
      rules: await inheritedRules(root, options, cache),
      depth: 0,
    },
  ];
  const deadline = options.clock() + options.limits.searchTimeBudgetMs;
  let scanned = 0;
  let cursor = 0;
  while (cursor < queue.length) {
    if (options.signal?.aborted) aborted();
    if (options.clock() > deadline) {
      options.budget.timedOut = true;
      return;
    }
    const { directory, rules, depth } = queue[
      cursor++
    ] as (typeof queue)[number];
    const declared = await ignoreRules(directory, options, cache);
    const effective = declared.length ? [...rules, ...declared] : rules;
    let entries: import("node:fs").Dirent[];
    try {
      entries = await readdir(directory, { withFileTypes: true });
    } catch {
      continue;
    }
    entries.sort((l, r) => (l.name < r.name ? -1 : l.name > r.name ? 1 : 0));
    for (const entry of entries) {
      if (++scanned > options.limits.searchMaxFiles) {
        options.budget.entriesExhausted = true;
        return;
      }
      if (
        entry.name === ".git" ||
        entry.name === "node_modules" ||
        entry.isSymbolicLink() ||
        (!options.allowSensitive && isSensitiveSegment(entry.name))
      )
        continue;
      const full = join(directory, entry.name);
      const isDirectory = entry.isDirectory();
      if (excluded(effective, full, isDirectory)) continue;
      const rel = relative(root, full).split(sep).join("/");
      if (isDirectory) {
        if (options.includeDirectories)
          yield { path: full, rel, isDirectory: true, depth: depth + 1 };
        if (options.maxDepth === undefined || depth + 1 < options.maxDepth)
          queue.push({ directory: full, rules: effective, depth: depth + 1 });
      } else if (entry.isFile()) {
        yield { path: full, rel, isDirectory: false, depth: depth + 1 };
      }
    }
  }
}

async function inheritedRules(
  root: string,
  options: WalkOptions,
  cache: Map<string, IgnoreRule[]>,
): Promise<IgnoreRule[]> {
  const from = options.inheritedFrom;
  if (!from) return [];
  const chain: string[] = [from];
  let directory = from;
  for (const part of relative(from, root).split(sep)) {
    if (!part || part === "..") return [];
    directory = join(directory, part);
    chain.push(directory);
  }
  chain.pop(); // the walk root declares its own ignore file when reached
  const rules: IgnoreRule[] = [];
  for (const ancestor of chain)
    rules.push(...(await ignoreRules(ancestor, options, cache)));
  return rules;
}

async function ignoreRules(
  directory: string,
  options: WalkOptions,
  cache: Map<string, IgnoreRule[]>,
): Promise<IgnoreRule[]> {
  const cached = cache.get(directory);
  if (cached) return cached;
  const rules: IgnoreRule[] = [];
  for (const name of IGNORE_FILES) {
    let text: string;
    try {
      text = await readFile(join(directory, name), "utf8");
    } catch {
      continue;
    }
    if (text.length > options.limits.ignoreFileMaxBytes) continue;
    for (const line of text.split("\n")) {
      const rule = parseIgnoreRule(line);
      if (rule) rules.push({ base: directory, ...rule });
    }
  }
  cache.set(directory, rules);
  return rules;
}

function parseIgnoreRule(line: string): Omit<IgnoreRule, "base"> | undefined {
  let text = line.replace(/\r$/, "").trim();
  if (!text || text.startsWith("#")) return undefined;
  let negate = false;
  if (text.startsWith("!")) {
    negate = true;
    text = text.slice(1);
  }
  let directoryOnly = false;
  if (text.endsWith("/")) {
    directoryOnly = true;
    text = text.slice(0, -1);
  }
  const anchored = text.includes("/");
  text = text.replace(/^\/+/, "");
  if (!text) return undefined;
  try {
    const body = globToPattern(text);
    return {
      pattern: new RegExp(anchored ? `^${body}$` : `(?:^|/)${body}$`),
      negate,
      directoryOnly,
    };
  } catch {
    return undefined; // a malformed ignore file must not break searching
  }
}

function globToPattern(glob: string): string {
  let pattern = "";
  for (let index = 0; index < glob.length; index++) {
    const char = glob[index] as string;
    if (char === "*" && glob[index + 1] === "*") {
      if (glob[index + 2] === "/") {
        pattern += "(?:[^/]*/)*";
        index += 2;
      } else {
        pattern += ".*";
        index += 1;
      }
    } else if (char === "*") pattern += "[^/]*";
    else if (char === "?") pattern += "[^/]";
    else if (char === "[") {
      const end = glob.indexOf("]", index + 1);
      if (end < 0) pattern += "\\[";
      else {
        pattern += `[${glob.slice(index + 1, end).replace(/^!/, "^")}]`;
        index = end;
      }
    } else if (char === "{") {
      const end = glob.indexOf("}", index + 1);
      if (end < 0) pattern += "\\{";
      else {
        pattern += `(?:${glob
          .slice(index + 1, end)
          .split(",")
          .map(globToPattern)
          .join("|")})`;
        index = end;
      }
    } else pattern += "\\^$.|?*+(){}".includes(char) ? `\\${char}` : char;
  }
  return pattern;
}

/** Compile a user glob; throws `CodingError` for an unusable one. */
export function compileGlob(glob: string): RegExp {
  try {
    return new RegExp(`^${globToPattern(glob)}$`);
  } catch {
    throw new CodingError(
      "invalid_glob",
      `invalid glob ${JSON.stringify(glob).slice(0, 200)}; use e.g. *.ts or **/*.test.tsx`,
    );
  }
}

/** A glob matches the relative path, or just the file name for bare globs. */
export function globMatches(glob: RegExp, rel: string): boolean {
  return glob.test(rel) || glob.test(basename(rel));
}

function excluded(rules: IgnoreRule[], file: string, isDirectory: boolean) {
  let drop = false;
  for (const rule of rules) {
    const path = relative(rule.base, file).split(sep).join("/");
    if (!path || path.startsWith("..") || (rule.directoryOnly && !isDirectory))
      continue;
    if (rule.pattern.test(path)) drop = !rule.negate;
  }
  return drop;
}

export interface SearchInput {
  /** Absolute canonical directory or file to search. */
  target: string;
  workspaceRoot: string;
  pattern: string;
  glob?: string | undefined;
  literal: boolean;
  ignoreCase: boolean;
  context: number;
  limit: number;
}

export interface SearchOutcome {
  text: string;
  truncated: boolean;
  matches: number;
}

function escapePattern(pattern: string): string {
  return pattern.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Bounded text search. Partial results come with a stated bound. */
export async function searchText(
  input: SearchInput,
  options: Omit<WalkOptions, "budget" | "inheritedFrom">,
): Promise<SearchOutcome> {
  const { limits } = options;
  if (input.pattern.length > limits.searchMaxPatternChars)
    throw new CodingError("search_limit", "pattern is too long");
  let regex: RegExp;
  try {
    regex = new RegExp(
      input.literal ? escapePattern(input.pattern) : input.pattern,
      input.ignoreCase ? "i" : "",
    );
  } catch {
    throw new CodingError(
      "invalid_pattern",
      `invalid regular expression ${JSON.stringify(input.pattern).slice(0, 200)}; pass literal: true to search for the text itself`,
    );
  }
  const glob = input.glob ? compileGlob(input.glob) : undefined;
  const kind = await stat(input.target).catch(() => null);
  if (!kind)
    throw new CodingError(
      "not_found",
      "nothing to search: path does not exist",
    );
  const isDir = kind.isDirectory();
  const budget: SearchBudget = {};
  const files: AsyncIterable<{ path: string; rel: string }> = isDir
    ? walkTree(input.target, {
        ...options,
        budget,
        inheritedFrom: input.workspaceRoot,
      })
    : (async function* () {
        yield { path: input.target, rel: basename(input.target) };
      })();
  const output: string[] = [];
  let bytes = 0;
  let outputBytes = 0;
  let matches = 0;
  let inputExhausted = false;
  const deadline = options.clock() + limits.searchTimeBudgetMs;
  const done = () =>
    matches >= input.limit || outputBytes >= limits.searchMaxOutputBytes;
  for await (const file of files) {
    if (options.signal?.aborted) aborted();
    if (options.clock() > deadline) {
      budget.timedOut = true;
      break;
    }
    if (glob && !globMatches(glob, file.rel)) continue;
    const info = await stat(file.path).catch(() => null);
    if (!info || !info.isFile() || info.size > limits.searchMaxFileBytes)
      continue;
    bytes += info.size;
    if (bytes > limits.searchMaxTotalBytes) {
      inputExhausted = true;
      break;
    }
    const text = await readFile(file.path, "utf8").catch(() => undefined);
    if (text === undefined || text.includes("\0")) continue;
    const lines = text.split("\n");
    for (let index = 0; index < lines.length; index++) {
      if (!regex.test((lines[index] as string).slice(0, 8192))) continue;
      const from = Math.max(0, index - input.context);
      const to = Math.min(lines.length - 1, index + input.context);
      for (let line = from; line <= to; line++) {
        const entry = `${file.rel}:${line + 1}:${(lines[line] as string).slice(0, limits.searchMaxLineChars)}`;
        output.push(entry);
        outputBytes += Buffer.byteLength(entry) + 1;
      }
      matches++;
      if (done()) break;
    }
    if (done()) break;
  }
  const reason = inputExhausted
    ? `${Math.round(bytes / 1024 / 1024)} MiB of input scanned`
    : budget.entriesExhausted
      ? `${limits.searchMaxFiles} entries scanned`
      : budget.timedOut
        ? `${limits.searchTimeBudgetMs / 1000}s of scanning elapsed`
        : matches >= input.limit
          ? `match limit ${input.limit} reached`
          : outputBytes >= limits.searchMaxOutputBytes
            ? "output limit reached"
            : "";
  const stopped = reason
    ? `${reason}; ${matches} match${matches === 1 ? "" : "es"} so far, narrow the path, glob or pattern`
    : "";
  const body = output.join("\n").slice(0, limits.searchMaxOutputBytes);
  const text = output.length
    ? `${body}${stopped ? `\n[stopped: ${stopped}]` : ""}`
    : `No matches found${stopped ? ` (${stopped})` : ""}`;
  return { text, truncated: stopped !== "", matches };
}

export interface FindOutcome {
  paths: string[];
  stopped: string;
}

/** Glob over the ignore-aware walk. */
export async function findFiles(
  pattern: string,
  target: string,
  workspaceRoot: string,
  limit: number,
  options: Omit<WalkOptions, "budget" | "inheritedFrom">,
): Promise<FindOutcome> {
  if (pattern.length > options.limits.searchMaxPatternChars)
    throw new CodingError("search_limit", "pattern is too long");
  const glob = compileGlob(pattern);
  const budget: SearchBudget = {};
  const paths: string[] = [];
  let hitLimit = false;
  for await (const entry of walkTree(target, {
    ...options,
    budget,
    inheritedFrom: workspaceRoot,
  })) {
    if (!globMatches(glob, entry.rel)) continue;
    paths.push(relative(workspaceRoot, entry.path).split(sep).join("/"));
    if (paths.length >= limit) {
      hitLimit = true;
      break;
    }
  }
  const reasons = [
    hitLimit ? `result limit ${limit} reached` : "",
    budget.entriesExhausted
      ? `${options.limits.searchMaxFiles} entries scanned`
      : "",
    budget.timedOut
      ? `${options.limits.searchTimeBudgetMs / 1000}s of scanning elapsed`
      : "",
  ].filter(Boolean);
  return { paths, stopped: reasons.join("; ") };
}
