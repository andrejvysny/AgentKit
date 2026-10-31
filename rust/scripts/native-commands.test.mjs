import { test } from "node:test";
import assert from "node:assert/strict";
import {
  mkdtempSync,
  mkdirSync,
  realpathSync,
  writeFileSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  assertNoRust,
  findExecutable,
  findNpmCli,
  pathEntries,
  toolchainFreePath,
  withPath,
} from "./native-commands.mjs";

test("Windows executable, npm CLI, and toolchain detection handle spaces", () => {
  const root = mkdtempSync(join(tmpdir(), "native commands "));
  try {
    const first = join(root, "runtime one");
    const second = join(root, "runtime two");
    mkdirSync(first);
    mkdirSync(second);
    writeFileSync(join(first, "node.cmd"), "");
    writeFileSync(join(second, "node.exe"), "");
    writeFileSync(join(second, "bun.exe"), "");
    assert.deepEqual(pathEntries(`"${first}";${second}`, "win32"), [
      first,
      second,
    ]);
    assert.equal(
      findExecutable("node", [first, second], "win32"),
      join(second, "node.exe"),
    );
    assert.equal(
      findExecutable("bun", [first, second], "win32"),
      join(second, "bun.exe"),
    );
    const npmDir = join(first, "node_modules", "npm", "bin");
    mkdirSync(npmDir, { recursive: true });
    writeFileSync(join(first, "npm.cmd"), "not a Node script");
    const cli = join(npmDir, "npm-cli.js");
    writeFileSync(cli, "process.exit(0);\n");
    assert.equal(
      findNpmCli(process.execPath, [first], "win32"),
      realpathSync(cli),
    );
    writeFileSync(join(second, "cargo.cmd"), "");
    assert.deepEqual(toolchainFreePath([first, second], "win32"), [first]);
    assert.throws(() => assertNoRust({}, [second], "win32"), /cargo\.cmd/);
    rmSync(join(second, "cargo.cmd"));
    writeFileSync(join(second, "rustc.exe"), "");
    assert.deepEqual(toolchainFreePath([first, second], "win32"), [first]);
    assert.throws(() => assertNoRust({}, [second], "win32"), /rustc\.exe/);
    const env = withPath(
      { Path: "old", OTHER: "yes" },
      [first, second],
      "win32",
    );
    assert.equal(env.PATH, `${first};${second}`);
    assert.equal(env.Path, undefined);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("missing npm CLI fails closed", () => {
  const root = mkdtempSync(join(tmpdir(), "native empty "));
  try {
    assert.equal(findExecutable("bun", [root]), undefined);
    assert.deepEqual(toolchainFreePath([".", root, root]), [root]);
    const node = join(root, "node.exe");
    writeFileSync(node, "");
    assert.throws(() => findNpmCli(node, [root], "win32"), /no usable npm-cli/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
