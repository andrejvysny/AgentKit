import { expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const body = (path: string): string => {
  const text = readFileSync(path, "utf8");
  return text.slice(text.indexOf("import type {"));
};

it("src/ports.ts mirrors packages/native/src/ports.ts", () => {
  const root = join(import.meta.dir, "..", "..");
  expect(body(join(root, "testing", "src", "ports.ts"))).toBe(
    body(join(root, "native", "src", "ports.ts")),
  );
});
