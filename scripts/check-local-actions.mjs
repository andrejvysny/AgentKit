#!/usr/bin/env node
import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
for (const directory of [".github/workflows", ".github/actions"]) {
  const path = join(root, directory);
  if (existsSync(path) && readdirSync(path).length > 0)
    throw new Error(`Hosted GitHub Actions are disabled: remove ${directory}`);
}
console.log("check-local-actions: no hosted workflows or composite actions");
