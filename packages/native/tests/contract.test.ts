import { expect, test } from "bun:test";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

/**
 * The error-code contract is the single source of truth shared between the
 * Rust binding and the TypeScript tests. Anything the binding can throw
 * must be a listed code; anything the tests name must exist. The generated
 * `.d.ts` must expose exactly the classes the wrapper imports.
 */

const packageRoot = new URL("..", import.meta.url).pathname;
const repoRoot = join(packageRoot, "..", "..");
const contractPath = join(
  repoRoot,
  "rust",
  "crates",
  "napi",
  "error-codes.json",
);

function contractCodes(): string[] {
  const parsed = JSON.parse(readFileSync(contractPath, "utf8")) as {
    codes?: unknown;
  };
  expect(Array.isArray(parsed.codes)).toBe(true);
  return (parsed.codes as unknown[]).filter(
    (code): code is string => typeof code === "string",
  );
}

test("the error-code contract is present, sorted, and deduplicated", () => {
  const codes = contractCodes();
  expect(codes.length).toBeGreaterThan(0);
  expect(new Set(codes).size).toBe(codes.length);
  const sorted = [...codes].sort();
  expect(codes).toEqual(sorted);
});

test("codes the wrapper surfaces are all in the contract", () => {
  const codes = new Set(contractCodes());
  const wrapper = readFileSync(join(packageRoot, "src", "index.ts"), "utf8");
  const mentioned = [...wrapper.matchAll(/NativeAgentError\("([a-z_]+)"/g)].map(
    (match) => match[1] as string,
  );
  expect(mentioned.length).toBeGreaterThan(0);
  for (const code of mentioned) {
    expect(codes.has(code)).toBe(true);
  }
});

test("generated d.ts exposes the classes the wrapper binds", () => {
  const dtsPath = join(packageRoot, "generated", "index.d.ts");
  if (!existsSync(dtsPath)) {
    throw new Error(
      "generated/index.d.ts is missing — run `bun run build:native` first",
    );
  }
  const dts = readFileSync(dtsPath, "utf8");
  for (const name of [
    "class NativeStore",
    "class SessionService",
    "class SubscriptionHandle",
    "function resolveCall",
    "function bindingSmoke",
  ]) {
    expect(dts).toContain(name);
  }
});

test("Rust and TypeScript share the same code list literally", () => {
  // The Rust side embeds the very same file via include_str!; a mismatch in
  // what the tests expect would silently mask a renamed code.
  const codes = contractCodes();
  for (const required of [
    "internal_error",
    "invalid_input",
    "closed",
    "callback_rejected",
    "already_owned",
    "session_not_found",
    "answer_conflict",
    "subscription_overflow",
  ]) {
    expect(codes).toContain(required);
  }
});
