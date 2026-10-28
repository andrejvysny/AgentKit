import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";

/**
 * Gating for the native binding tests.
 *
 * The compiled `.node`, the loader, and the `.d.ts` are build artifacts
 * (`bun run build:native`), not committed files. Ordinary TypeScript CI
 * intentionally has no native build; the qualification job that DOES build
 * it sets `AGENTKIT_REQUIRE_NATIVE=1`, where a missing binary is a hard
 * failure rather than a silent skip.
 */

const packageRoot = join(import.meta.dir, "..", "..");
const generatedDir = join(packageRoot, "generated");

export const required: boolean = process.env.AGENTKIT_REQUIRE_NATIVE === "1";

const loaderPresent = existsSync(join(generatedDir, "index.js"));
const dtsPresent = existsSync(join(generatedDir, "index.d.ts"));
const binaryPresent = (() => {
  try {
    return readdirSync(generatedDir).some((name) => name.endsWith(".node"));
  } catch {
    return false;
  }
})();

export const available: boolean = loaderPresent && dtsPresent && binaryPresent;

export const missing: string[] = [
  ...(loaderPresent ? [] : ["generated/index.js"]),
  ...(dtsPresent ? [] : ["generated/index.d.ts"]),
  ...(binaryPresent ? [] : ["generated/*.node"]),
];
