#!/usr/bin/env bun
/** Export reviewable Rust parity inputs from the TypeScript authority. */
import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import * as schemas from "../packages/contracts/src/schemas.js";
import {
  AiRunEventSchema,
  CONTRACT_VERSION,
  REST_API_VERSION,
  REST_ROUTES,
} from "../packages/contracts/src/index.js";
import {
  deriveIdempotentTaskId,
  deriveRegenerateTaskId,
  turnFingerprint,
} from "../packages/transport-http/src/idempotency.js";
import {
  GOLDEN_TRACE_NAMES,
  loadGoldenTrace,
} from "../packages/testing/src/golden/golden.js";

const ROOT = resolve(import.meta.dir, "..");
export const FIXTURE_DIR = join(ROOT, "rust/fixtures");
export const BASELINE_SHA = "36577829cfbfd78e5b0492d826f46ba6131835b6";

/** Sort object keys only; array order and all JSON schema semantics survive. */
export function canonicalJson(value: unknown): string {
  const plain: unknown = JSON.parse(JSON.stringify(value));
  const sort = (item: unknown): unknown => {
    if (Array.isArray(item)) return item.map(sort);
    if (item !== null && typeof item === "object") {
      return Object.fromEntries(
        Object.entries(item)
          .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
          .map(([key, entry]) => [key, sort(entry)]),
      );
    }
    return item;
  };
  return `${JSON.stringify(sort(plain), null, 2)}\n`;
}

async function idempotencyVectors(): Promise<unknown> {
  const inputs = [
    { chatId: "chat-1", messageId: "message-1", key: "retry-1" },
    { chatId: "chat-2", messageId: "message-1", key: "retry-1" },
    { chatId: "chat-1", messageId: "message-2", key: "retry-1" },
    { chatId: "chat-1", messageId: "message-1", key: "message-1:retry-1" },
    { chatId: "chat-雪", messageId: "message-🙂", key: "café:雪🙂" },
    { chatId: "chat-1", messageId: "message-1", key: "" },
  ];
  const vectors = await Promise.all(
    inputs.map(async (input) => ({
      ...input,
      submitTaskId: await deriveIdempotentTaskId(input.chatId, input.key),
      regenerateTaskId: await deriveRegenerateTaskId(
        input.chatId,
        input.messageId,
        input.key,
      ),
    })),
  );
  const parts: Record<string, unknown>[] = [
    { content: "hello", providerId: "p1", model: "m1" },
    { model: "m1", content: "hello", providerId: "p1" },
    { content: [{ text: "雪🙂", type: "text" }], metadata: { z: 1, a: null } },
    { content: "changed", providerId: "p1", model: "m1" },
    { values: [1, 2], nested: { z: false, a: "é" } },
    { values: [2, 1], nested: { a: "é", z: false } },
    { nullable: null, ignored: undefined },
    { numbers: [1.0, -0.0, 1e-6, 1e-7, 1e20, 1e21] },
    { "\u{10000}": "supplementary", "\uE000": "BMP" },
    { "2": "two", "10": "ten" },
  ];
  return {
    source: "packages/transport-http/src/idempotency.ts",
    encoding: "UTF-8",
    hash: "SHA-256",
    submitPreimage: "<chatId>:<key>",
    regeneratePreimage: "<chatId>:<messageId>:<key>",
    vectors,
    fingerprints: await Promise.all(
      parts.map(async (input) => ({
        input,
        fingerprint: await turnFingerprint(input),
      })),
    ),
    limitations: [
      "Empty key tests derivation only; HTTP requires a non-empty key.",
      "Colon-containing identities are not injective; shipped hosts generate colon-free identities.",
      "Fingerprint vectors cover JSON inputs; JavaScript undefined properties are omitted.",
    ],
  };
}

/** No timestamps, git queries, network calls or random identities are emitted. */
export async function buildFixtures(): Promise<Map<string, string>> {
  const files = new Map<string, string>();
  files.set("schemas.json", canonicalJson(schemas));
  files.set(
    "routes.json",
    canonicalJson({
      apiVersion: REST_API_VERSION,
      routes: Object.entries(REST_ROUTES).map(([operation, route]) => ({
        operation,
        ...route,
        requiredHeaders: ["submitMessage", "regenerateMessage"].includes(
          operation,
        )
          ? ["Idempotency-Key"]
          : [],
        ...(operation === "streamRun"
          ? {
              responseContentType: "text/event-stream",
              resumeHeader: "Last-Event-ID",
            }
          : {}),
      })),
    }),
  );
  files.set(
    "run-events.json",
    canonicalJson({
      contractVersion: CONTRACT_VERSION,
      variants: AiRunEventSchema.anyOf.map((schema) => ({
        type: schema.properties.type.const,
        schema,
      })),
    }),
  );
  files.set("idempotency.json", canonicalJson(await idempotencyVectors()));
  for (const name of GOLDEN_TRACE_NAMES) {
    files.set(
      `golden-traces/${name}.json`,
      canonicalJson(loadGoldenTrace(name)),
    );
  }
  const hashes = Object.fromEntries(
    [...files].map(([path, body]) => [
      path,
      createHash("sha256").update(body).digest("hex"),
    ]),
  );
  files.set(
    "manifest.json",
    canonicalJson({
      formatVersion: 1,
      baselineSha: BASELINE_SHA,
      contractVersion: CONTRACT_VERSION,
      schemaCount: Object.keys(schemas).length,
      routeCount: Object.keys(REST_ROUTES).length,
      eventVariantCount: AiRunEventSchema.anyOf.length,
      goldenTraceCount: GOLDEN_TRACE_NAMES.length,
      independentlyCheckedFixtures: ["conformance/", "schema-validation.json"],
      sha256: hashes,
    }),
  );
  return files;
}

async function existingFiles(
  directory: string,
  prefix = "",
): Promise<string[]> {
  const entries = await readdir(directory, { withFileTypes: true }).catch(
    (error: NodeJS.ErrnoException) => {
      if (error.code === "ENOENT") return [];
      throw error;
    },
  );
  const paths = await Promise.all(
    entries.map(async (entry) => {
      const path = `${prefix}${entry.name}`;
      return entry.isDirectory()
        ? existingFiles(join(directory, entry.name), `${path}/`)
        : [path];
    }),
  );
  return paths.flat();
}

/** Check mode reports missing, changed and stale files without writing. */
export async function exportFixtures(
  directory = FIXTURE_DIR,
  check = false,
): Promise<string[]> {
  const files = await buildFixtures();
  const drift: string[] = [];
  for (const [path, body] of files) {
    const target = join(directory, path);
    if (check) {
      const actual = await readFile(target, "utf8").catch(
        (error: NodeJS.ErrnoException) => {
          if (error.code === "ENOENT") return null;
          throw error;
        },
      );
      if (actual !== body) drift.push(path);
    } else {
      await mkdir(dirname(target), { recursive: true });
      await writeFile(target, body);
    }
  }
  const unexpected = (await existingFiles(directory)).filter(
    (path) =>
      !files.has(path) &&
      !path.startsWith("conformance/") &&
      path !== "schema-validation.json",
  );
  return [...drift, ...unexpected].sort();
}

if (import.meta.main) {
  const args = process.argv.slice(2);
  if (args.some((argument) => argument !== "--check")) {
    throw new Error("Usage: bun scripts/export-rust-fixtures.ts [--check]");
  }
  const check = args.includes("--check");
  const drift = await exportFixtures(FIXTURE_DIR, check);
  if (drift.length > 0) {
    console.error(`Rust fixture drift: ${drift.join(", ")}`);
    process.exitCode = 1;
  } else {
    console.log(check ? "Rust fixtures current." : "Rust fixtures exported.");
  }
}
