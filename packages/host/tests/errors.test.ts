import { describe, expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { HOST_ERROR_CODES } from "../src/errors.js";

/**
 * `HOST_ERROR_CODES` is meant to be the closed, exhaustive set of codes the
 * named subclasses in `errors.ts` use — kept in sync at compile time by
 * `NamedHostError` typing its `code` parameter as `HostErrorCode` (see that
 * file). This test is the belt-and-suspenders half: it reads the SOURCE TEXT
 * of `errors.ts` directly (not the compiled types, which a change here could
 * not see) and asserts every `super("<code>"` literal it finds is present in
 * `HOST_ERROR_CODES`.
 *
 * Mutation-killing: add a new `NamedHostError` subclass and forget to add its
 * code to `HOST_ERROR_CODES` — `tsc` already refuses to compile it (the
 * subclass's `super(...)` call type-errors against `HostErrorCode`), but if
 * that guard were ever loosened or bypassed (e.g. an `as HostErrorCode`
 * assertion slipped past review), this scan still fails the union closed.
 */
const ERRORS_SOURCE_PATH = fileURLToPath(
  new URL("../src/errors.ts", import.meta.url),
);

/** Every `super("<code>"` literal in the file, in source order, duplicates included. */
function extractSuperCodes(source: string): string[] {
  const pattern = /\bsuper\(\s*"([a-z_]+)"/g;
  const codes: string[] = [];
  for (const match of source.matchAll(pattern)) {
    const code = match[1];
    if (code !== undefined) codes.push(code);
  }
  return codes;
}

describe("HOST_ERROR_CODES", () => {
  const source = readFileSync(ERRORS_SOURCE_PATH, "utf8");
  const superCodes = extractSuperCodes(source);

  it('found at least one super("<code>" call to scan (sanity: the regex still matches this file\'s shape)', () => {
    expect(superCodes.length).toBeGreaterThan(0);
  });

  it("contains every code passed to super(...) by a NamedHostError subclass in errors.ts", () => {
    const known = new Set<string>(HOST_ERROR_CODES);
    const missing = superCodes.filter((code) => !known.has(code));
    expect(missing).toEqual([]);
  });

  it("has no duplicate codes", () => {
    expect(new Set(HOST_ERROR_CODES).size).toBe(HOST_ERROR_CODES.length);
  });

  it("has no codes that errors.ts never actually throws (the union isn't padded)", () => {
    const thrown = new Set(superCodes);
    const unused = HOST_ERROR_CODES.filter((code) => !thrown.has(code));
    expect(unused).toEqual([]);
  });
});
