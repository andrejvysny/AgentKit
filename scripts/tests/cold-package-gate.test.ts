import { afterAll, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dir, "../..");
const script = join(root, "scripts/qualify-cold-packages.mjs");
const temporary = mkdtempSync(join(tmpdir(), "agentkit-cold-boundary-"));

afterAll(() => rmSync(temporary, { recursive: true, force: true }));

it("refuses an existing evidence directory without changing its contents", () => {
  const marker = join(temporary, "retained-evidence.json");
  writeFileSync(marker, '{"status":"PASS"}\n');
  const result = spawnSync("node", [script, temporary], { encoding: "utf8" });
  expect(result.status).not.toBe(0);
  expect(result.stderr).toContain("new directory outside the checkout");
  expect(readFileSync(marker, "utf8")).toBe('{"status":"PASS"}\n');
});

it("refuses an output path entering the checkout through a symlink", () => {
  const link = join(temporary, "checkout");
  symlinkSync(root, link, process.platform === "win32" ? "junction" : "dir");
  const name = `.cold-package-qualification-refused-${process.pid}`;
  expect(existsSync(join(root, name))).toBe(false);
  const result = spawnSync("node", [script, join(link, name)], {
    encoding: "utf8",
  });
  expect(result.status).not.toBe(0);
  expect(result.stderr).toContain("new directory outside the checkout");
  expect(existsSync(join(root, name))).toBe(false);
});

it(
  "does not accept a signalled extractor process as the expected build failure",
  () => {
    const fixture = join(temporary, "signalled-extractor");
    mkdirSync(fixture);
    const preload = join(fixture, "preload.mjs");
    writeFileSync(
      preload,
      `
import childProcess from "node:child_process";
import { syncBuiltinESMExports } from "node:module";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
const original = childProcess.spawnSync;
childProcess.spawnSync = (command, args, options) => {
  if (args[0] === "scripts/check-public-api.mjs") {
    const directory = join(options.cwd, "node_modules/.cache/agentkit-api");
    mkdirSync(directory, { recursive: true });
    writeFileSync(join(directory, "native.api.md"), "fixture report\\n");
    return { status: 0, signal: null, stdout: "fixture report\\n", stderr: "" };
  }
  if (command !== "bun" && command !== "cargo") return original(command, args, options);
  if (options.env.NODE_OPTIONS?.startsWith("--experimental-loader="))
    return { status: null, signal: "SIGTERM", stdout: "", stderr: "api-extractor failed to roll up" };
  if (args.includes("packages/native") && args.at(-1) === "build") {
    const directory = join(options.cwd, "packages/native/dist");
    mkdirSync(directory, { recursive: true });
    writeFileSync(join(directory, "index.d.ts"), "export declare const fixture: string;\\n");
  }
  return { status: 0, signal: null, stdout: "fixture runtime\\n", stderr: "" };
};
syncBuiltinESMExports();
`,
    );
    const output = join(fixture, "evidence");
    const result = spawnSync(
      "node",
      ["--import", pathToFileURL(preload).href, script, output],
      {
        encoding: "utf8",
        timeout: 60_000,
      },
    );
    expect(result.status).toBe(1);
    const evidence = JSON.parse(
      readFileSync(join(output, "qualification.json"), "utf8"),
    ) as {
      status: string;
      commands: {
        label: string;
        status: string;
        exitCode: number | null;
        signal: string | null;
      }[];
    };
    expect(evidence.status).toBe("INCONCLUSIVE");
    expect(evidence.commands.at(-1)).toMatchObject({
      label: "extractor-failure-build",
      status: "INCONCLUSIVE",
      exitCode: null,
      signal: "SIGTERM",
    });
  },
  { timeout: 60_000 },
);
