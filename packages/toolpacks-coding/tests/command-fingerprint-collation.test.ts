/**
 * The Rust fingerprint port reuses the shared collation order fixture
 * (`rust/fixtures/coding-fingerprint/env-order.json`) instead of
 * `localeCompare`. These pins keep that table honest against both runtimes'
 * actual collation, and against the TypeScript ENV_NAMES universe — a drift
 * here would silently desynchronize digests between TS and Rust.
 */
import { expect, describe, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const FIXTURE = join(
  root,
  "rust",
  "fixtures",
  "coding-fingerprint",
  "env-order.json",
);

// The name universe of src/command-fingerprint.ts, copied deliberately:
// drift between this list and the authority is a pin failure, not a silent
// fixture gap.
const ENV_NAMES = [
  "PATH",
  "HOME",
  "SHELL",
  "BASH_ENV",
  "ENV",
  "LANG",
  "LC_ALL",
  "LC_CTYPE",
  "NODE_ENV",
  "NODE_OPTIONS",
  "NODE_PATH",
  "BUN_OPTIONS",
  "BUN_INSTALL",
  "PYTHONPATH",
  "PYTHONHOME",
  "VIRTUAL_ENV",
  "PYTEST_ADDOPTS",
  "UV_PROJECT_ENVIRONMENT",
  "CARGO_HOME",
  "RUSTUP_HOME",
  "RUSTUP_TOOLCHAIN",
  "RUSTFLAGS",
  "RUSTDOCFLAGS",
  "GOPATH",
  "GOROOT",
  "GOFLAGS",
  "GOWORK",
  "GOTOOLCHAIN",
  "MAKEFLAGS",
  "LD_LIBRARY_PATH",
  "LD_PRELOAD",
  "DYLD_LIBRARY_PATH",
  "DYLD_INSERT_LIBRARIES",
  "npm_config_prefix",
  "npm_config_userconfig",
  "npm_config_registry",
];

describe("fingerprint environment collation", () => {
  test("the shared fixture equals this runtime's localeCompare order", () => {
    const fixture = JSON.parse(readFileSync(FIXTURE, "utf8")) as string[];
    expect(fixture).toEqual([...ENV_NAMES].sort((a, b) => a.localeCompare(b)));
  });

  test("the fixture covers exactly the ENV_NAMES universe", () => {
    const fixture = JSON.parse(readFileSync(FIXTURE, "utf8")) as string[];
    expect([...fixture].sort()).toEqual([...ENV_NAMES].sort());
    expect(new Set(fixture).size).toBe(ENV_NAMES.length);
  });
});
