import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";

const generated = new Set([
  ".git",
  "node_modules",
  "dist",
  "coverage",
  ".DS_Store",
  "snapshot-provenance.json",
]);

function copySource(source: string, target: string): void {
  mkdirSync(target, { recursive: true });
  for (const entry of readdirSync(source, { withFileTypes: true })) {
    if (generated.has(entry.name) || entry.name.endsWith(".tsbuildinfo"))
      continue;
    const input = join(source, entry.name);
    const output = join(target, entry.name);
    if (entry.isDirectory()) copySource(input, output);
    else if (entry.isFile()) {
      mkdirSync(dirname(output), { recursive: true });
      copyFileSync(input, output);
    } else throw new Error(`Fixture source must be regular: ${input}`);
  }
}

export function sourceFixture(source: string, destination: string): string {
  copySource(source, destination);
  for (const args of [
    ["init", "--quiet"],
    ["add", "--all"],
    [
      "-c",
      "core.hooksPath=/dev/null",
      "-c",
      "user.name=Release test",
      "-c",
      "user.email=release-test@example.invalid",
      "commit",
      "--quiet",
      "-m",
      "Create isolated release test source",
    ],
  ]) {
    const result = spawnSync("git", args, {
      cwd: destination,
      encoding: "utf8",
    });
    if (result.status !== 0) throw new Error(result.stderr);
  }
  return destination;
}
