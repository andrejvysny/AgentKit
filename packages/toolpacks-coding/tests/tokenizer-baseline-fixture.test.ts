/**
 * The Rust tokenizer port is pinned to the shared fixture
 * (`rust/fixtures/coding-fingerprint/tokenizer.json`), whose expected values
 * are the frozen TypeScript baseline's actual outputs (generated from the
 * 77fc558 sources; Node 22.20.0 and Bun 1.4.4 agree byte-for-byte). The
 * Rust side consumes the same file in
 * `rust/crates/toolpacks-coding/src/command_fingerprint.rs`. These pins run
 * the CURRENT TypeScript implementation against the same table, case for
 * case, so drift in `shellWords`, the fingerprint token stream,
 * `directScriptFiles` (accepts AND exact refusal texts) or `scriptExecutable`
 * fails here first — the TS baseline is the truth, so every case must pass.
 */
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { fingerprintTokens } from "../src/command-fingerprint.js";
import {
  directScriptFiles,
  scriptExecutable,
} from "../src/command-script-inputs.js";
import { executableName, shellWords } from "../src/command-words.js";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const FIXTURE = join(
  root,
  "rust",
  "fixtures",
  "coding-fingerprint",
  "tokenizer.json",
);

type Case = {
  input: string;
  words: string[];
  tokens: string[];
  files: { ok: string[] } | { error: string };
  scriptExecutable: { ok: string | null };
};

type Fixture = {
  source: string;
  runtimes: string[];
  agreement: string;
  cases: Case[];
  executableNames: { input: string; name: string }[];
};

const fixture = JSON.parse(readFileSync(FIXTURE, "utf8")) as Fixture;

describe("tokenizer baseline fixture", () => {
  test("the fixture carries its provenance and non-empty case tables", () => {
    expect(fixture.source).toContain("77fc558");
    expect(fixture.runtimes).toEqual(["Node 22.20.0", "Bun 1.4.4"]);
    expect(fixture.agreement).toContain("byte-identical");
    expect(fixture.cases.length).toBeGreaterThan(0);
    expect(fixture.executableNames.length).toBeGreaterThan(0);
  });

  for (const [index, case_] of fixture.cases.entries()) {
    const label = `case ${index}: ${JSON.stringify(case_.input)}`;

    test(`${label} — shellWords`, () => {
      expect(shellWords(case_.input)).toEqual(case_.words);
    });

    test(`${label} — fingerprint tokens`, () => {
      expect(fingerprintTokens(case_.input)).toEqual(case_.tokens);
    });

    test(`${label} — directScriptFiles`, () => {
      if ("error" in case_.files) {
        const expected = case_.files.error;
        let message: string | undefined;
        try {
          directScriptFiles(case_.input);
        } catch (error) {
          message = error instanceof Error ? error.message : String(error);
        }
        expect(message).toBe(expected);
        return;
      }
      expect(directScriptFiles(case_.input)).toEqual(case_.files.ok);
    });

    test(`${label} — scriptExecutable`, () => {
      const expected =
        case_.scriptExecutable.ok === null
          ? undefined
          : case_.scriptExecutable.ok;
      expect(scriptExecutable(case_.input)).toBe(expected);
    });
  }

  for (const [index, case_] of fixture.executableNames.entries()) {
    test(`executableName ${index}: ${JSON.stringify(case_.input)}`, () => {
      expect(executableName(case_.input)).toBe(case_.name);
    });
  }
});
