import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";

/**
 * Gating for the native binding tests.
 *
 * The Rust runtime is the only backend, so a missing compiled binding is a
 * hard failure everywhere (the M2.1 A4 rule): `bun run build:native` (or an
 * installed platform package) is a prerequisite of running the tests at all.
 * `AGENTKIT_REQUIRE_NATIVE=1` remains the explicit qualification flag;
 * absence of the binary fails regardless of it.
 */

const packageRoot = join(import.meta.dir, "..", "..");
const generatedDir = join(packageRoot, "generated");

export const required: boolean = true;

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
