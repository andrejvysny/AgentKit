import { expect, test } from "bun:test";
import { parseParitySummary } from "./parity-summary.mjs";

test("counts come from the trailing summary, not an earlier lookalike line", () => {
  const output = [
    "packages/host/tests/slow.test.ts:",
    " 1161 pass (early progress/log noise, must be ignored)",
    " 292 expect() calls",
    "",
    "packages/host/tests/other.test.ts:",
    " 676 pass",
    " 11 skip",
    " 0 fail",
    "Ran 687 tests across 96 files. [4.85s]",
  ].join("\n");
  expect(parseParitySummary(output)).toEqual({
    pass: 676,
    fail: 0,
    skip: 11,
    todo: 0,
    complete: true,
    partial: false,
  });
});

test("a killed lane reports complete:false and counts only real markers", () => {
  const output = [
    "packages/host/tests/a.test.ts:",
    "(fail) submit streams a tool call > exceeds the budget",
    "(fail) submit streams a tool call > retries once",
    " 5 pass (progress line, not a summary)",
  ].join("\n");
  const parsed = parseParitySummary(output);
  expect(parsed.complete).toBeFalse();
  expect(parsed.partial).toBeTrue();
  expect(parsed.fail).toBe(2);
  expect(parsed.pass).toBe(0);
});

test("absent kinds count zero without inventing a summary", () => {
  expect(parseParitySummary("Ran 3 tests across 1 file.")).toEqual({
    pass: 0,
    fail: 0,
    skip: 0,
    todo: 0,
    complete: true,
    partial: false,
  });
});
