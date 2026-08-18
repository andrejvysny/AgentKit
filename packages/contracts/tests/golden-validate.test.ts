import { describe, expect, it } from "bun:test";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import Ajv from "ajv";
import type { TSchema } from "@sinclair/typebox";
import { AiRunEventSchema } from "../src/index.js";

/**
 * @agentkit/testing's golden traces, read directly off disk rather than via
 * `@agentkit/testing` (which depends on `@agentkit/contracts`, not the other
 * way around — importing it here would be a package-boundary cycle).
 * Resolved from `import.meta.dir` so this test works under `bun test`
 * regardless of which directory it's invoked from.
 */
const TRACES_DIR = join(
  import.meta.dir,
  "..",
  "..",
  "testing",
  "src",
  "golden",
  "traces",
);

const asJson = (schema: TSchema): object =>
  JSON.parse(JSON.stringify(schema)) as object;

describe("golden traces validate against AiRunEventSchema", () => {
  const traceFiles = readdirSync(TRACES_DIR).filter((f) =>
    f.endsWith(".json"),
  );

  it("finds the committed trace files", () => {
    expect(traceFiles.length).toBeGreaterThan(0);
  });

  const validate = new Ajv({ strict: false, allErrors: true }).compile(
    asJson(AiRunEventSchema),
  );

  for (const file of traceFiles) {
    it(`every event in ${file} validates`, () => {
      const events = JSON.parse(
        readFileSync(join(TRACES_DIR, file), "utf8"),
      ) as unknown[];
      expect(events.length).toBeGreaterThan(0);
      for (const event of events) {
        const ok = validate(event);
        if (!ok) {
          throw new Error(
            `${file}: event failed AiRunEventSchema validation: ${JSON.stringify(
              validate.errors,
            )}\nEvent: ${JSON.stringify(event)}`,
          );
        }
        expect(ok).toBe(true);
      }
    });
  }
});
