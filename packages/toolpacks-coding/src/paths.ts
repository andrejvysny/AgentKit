/**
 * Workspace confinement and the sensitive-path denylist.
 *
 * Adapted from OneMind (https://github.com/andrejvysny/OneMind), commit
 * 51abbbd, path `src-ts/src/code/exploration.ts` (lines 14-128:
 * `resolveInsideRoot`, the sensitive directory/extension/name lists and the
 * `.env*` template handling).
 * Copyright (c) 2026 OneMind contributors. MIT License.
 * Changes: typed `CodingError` codes, `allowSensitive`, `.git` write refusal,
 * prefix names (`id_rsa*`, `credentials*`, `secrets*`), root canonicalisation.
 */
import { lstat, realpath } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { CodingError } from "./errors.js";

const SENSITIVE_DIRECTORIES = new Set([
  ".git",
  ".ssh",
  ".aws",
  ".gnupg",
  ".kube",
  ".docker",
  ".azure",
  ".terraform.d",
]);
const SENSITIVE_EXTENSIONS = new Set([
  ".pem",
  ".key",
  ".p12",
  ".pfx",
  ".jks",
  ".keystore",
  ".p8",
  ".jwk",
  ".kdbx",
]);
const SENSITIVE_NAMES = new Set([
  ".netrc",
  ".npmrc",
  ".pypirc",
  ".git-credentials",
  ".htpasswd",
  ".pgpass",
]);
/** A name that merely starts with one of these is sensitive (`id_rsa.pub`). */
const SENSITIVE_PREFIXES = [
  "id_rsa",
  "id_dsa",
  "id_ecdsa",
  "id_ed25519",
  "id_ed448",
  "credentials",
  "secrets",
  "service-account",
];
/** Terraform state holds provider credentials in clear (`x.tfstate.backup`). */
const SENSITIVE_INFIXES = [".tfstate"];
/** Committed `.env` templates document configuration; real env files do not. */
const TEMPLATE_SUFFIXES = [
  ".example",
  ".sample",
  ".template",
  ".tpl",
  ".dist",
  ".default",
];

/** Glob excludes mirroring the denylist, for tools that cannot filter per file. */
export const SENSITIVE_GIT_EXCLUDES: readonly string[] = [
  ...SENSITIVE_DIRECTORIES,
]
  .filter((name) => name !== ".git")
  .map((name) => `:(exclude,glob)**/${name}/**`)
  .concat(
    [...SENSITIVE_EXTENSIONS].map((ext) => `:(exclude,glob)**/*${ext}`),
    [...SENSITIVE_NAMES].map((name) => `:(exclude,glob)**/${name}`),
    SENSITIVE_PREFIXES.map((name) => `:(exclude,glob)**/${name}*`),
    SENSITIVE_INFIXES.map((infix) => `:(exclude,glob)**/*${infix}*`),
    [":(exclude,glob)**/.env", ":(exclude,glob)**/.env.*"],
    [":(exclude,glob)**/*.env"],
  );

/** True when one path segment names a credential, key or git metadata. */
export function isSensitiveSegment(raw: string): boolean {
  const segment = raw.trim().toLowerCase();
  if (!segment || segment === "." || segment === "**") return false;
  if (SENSITIVE_DIRECTORIES.has(segment) || SENSITIVE_NAMES.has(segment))
    return true;
  if (SENSITIVE_PREFIXES.some((prefix) => segment.startsWith(prefix)))
    return true;
  if (SENSITIVE_INFIXES.some((infix) => segment.includes(infix))) return true;
  if (segment.startsWith(".env"))
    return !TEMPLATE_SUFFIXES.some((suffix) => segment.endsWith(suffix));
  if (segment.endsWith(".env")) return true;
  const dot = segment.lastIndexOf(".");
  return dot > 0 && SENSITIVE_EXTENSIONS.has(segment.slice(dot));
}

function segmentsOf(path: string): string[] {
  return path.split(/[\\/]/).filter((part) => part.length > 0);
}

export function isGitSegment(segment: string): boolean {
  return segment.toLowerCase() === ".git";
}

/** True when any segment of a root-relative path is sensitive. */
export function isSensitivePath(path: string): boolean {
  return segmentsOf(path).some(isSensitiveSegment);
}

export interface ConfinedPath {
  /** Canonical workspace root. */
  root: string;
  /** Canonical target: existing parents resolved, missing children appended. */
  path: string;
  /** Target relative to `root`; `""` for the root itself. */
  relativePath: string;
}

export type PathAccess = "read" | "write";

export interface ConfineOptions {
  access: PathAccess;
  allowSensitive: boolean;
}

function denied(code: string, message: string): never {
  throw new CodingError(code, message);
}

function escapes(rel: string): boolean {
  return rel === ".." || rel.startsWith(`..${sep}`) || isAbsolute(rel);
}

/** Canonical root, or a clear error when the workspace is not usable. */
export async function canonicalRoot(root: string): Promise<string> {
  try {
    return await realpath(root);
  } catch {
    throw new CodingError(
      "workspace_unavailable",
      "the workspace root does not exist or cannot be resolved",
    );
  }
}

/**
 * Resolve `raw` against the canonical root and refuse anything that leaves it.
 * Symlinks are followed to their real target; a dangling symlink counts as an
 * escape because writing through it would create its target somewhere else.
 */
export async function resolveInsideRoot(
  root: string,
  raw: unknown,
): Promise<ConfinedPath> {
  if (typeof raw !== "string" || raw.length === 0 || raw.length > 4096)
    denied("path_invalid", "path must be a non-empty string (max 4096)");
  if (raw.includes("\0")) denied("path_invalid", "path contains a NUL byte");
  if (raw.startsWith("~"))
    denied("path_invalid", "paths starting with ~ are not supported");
  const target = resolve(root, raw);
  let existing = target;
  const missing: string[] = [];
  while (true) {
    try {
      existing = await realpath(existing);
      break;
    } catch {
      if ((await lstat(existing).catch(() => null))?.isSymbolicLink())
        denied(
          "path_dangling_symlink",
          `${raw} is a dangling symlink; refusing to follow it`,
        );
      const parent = dirname(existing);
      if (parent === existing)
        denied("path_outside_root", `${raw} is outside the workspace`);
      missing.unshift(existing.slice(parent.length + 1));
      existing = parent;
    }
  }
  const path = join(existing, ...missing);
  const relativePath = relative(root, path);
  if (escapes(relativePath))
    denied("path_outside_root", `${raw} is outside the workspace`);
  return { root, path, relativePath };
}

/**
 * The full gate every path argument passes through: confinement, the `.git`
 * write ban, and the sensitive denylist. The denylist is checked on the raw
 * segments AND on the resolved root-relative path, so a symlink inside the
 * workspace that points at `.env` is refused too.
 */
export async function confine(
  root: string,
  raw: unknown,
  options: ConfineOptions,
): Promise<ConfinedPath> {
  const confined = await resolveInsideRoot(root, raw);
  const lexical = relative(root, resolve(root, String(raw)));
  const candidates = [confined.relativePath, lexical].filter(
    (rel) => !escapes(rel),
  );
  const segments = candidates.flatMap(segmentsOf);
  if (options.access === "write" && segments.some(isGitSegment))
    denied("path_git_write", "writes under .git are never allowed");
  if (!options.allowSensitive) {
    const hit = segments.find(isSensitiveSegment);
    if (hit !== undefined)
      denied(
        "path_sensitive",
        `${String(raw)} names a sensitive path (${hit}); refused`,
      );
  }
  return confined;
}
