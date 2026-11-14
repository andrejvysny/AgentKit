import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import type { CapturedSession } from "./replay";
import { report } from "./report";

const args = process.argv.slice(2);
if (args.length !== 2)
  throw new Error("usage: bun score.ts CAPTURE.json NEW_REPORT.json");
const [input, output] = args;
if (!input || !output) throw new Error("input and output are required");
const repository = fileURLToPath(new URL("../../../", import.meta.url));
const manifest = fileURLToPath(
  new URL("../../../rust/Cargo.toml", import.meta.url),
);
const inputPath = resolve(input);
const before = readFileSync(inputPath);
const checked = spawnSync(
  "cargo",
  [
    "+1.97.0",
    "run",
    "--manifest-path",
    manifest,
    "--locked",
    "-p",
    "agentkit-runtime",
    "--example",
    "context_capture",
    "--",
    "--replay",
    inputPath,
  ],
  { cwd: repository, stdio: "inherit" },
);
if (checked.error || checked.status !== 0)
  throw new Error("assembly verifier failed");
if (!before.equals(readFileSync(inputPath)))
  throw new Error("capture changed during verification");
const parsed: unknown = JSON.parse(before.toString("utf8"));
if (!Array.isArray(parsed) || parsed.length === 0)
  throw new Error("expected captured session array");
const result = report(parsed as CapturedSession[], {
  lifetime_ms: 300_000,
  minimum_tokens: 256,
});
writeFileSync(output, `${JSON.stringify(result, null, 2)}\n`, { flag: "wx" });
