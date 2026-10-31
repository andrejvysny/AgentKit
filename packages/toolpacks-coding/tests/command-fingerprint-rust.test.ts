/**
 * Differential qualification of the Rust standing-approval fingerprint
 * against the TypeScript authority (`src/command-fingerprint.ts`). Both
 * sides compute the digest over the SAME tree in the SAME process lifetime,
 * so executable identity fields (dev/ino/mode/size/mtimeNs/ctimeNs) match
 * byte for byte. Gated like the other cross-language conformance suites.
 */
import { describe, expect, it } from "bun:test";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { commandFingerprint } from "../src/command-fingerprint.js";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const differential =
  process.env.AGENTKIT_RUN_RUST_CONFORMANCE === "1" ? it : it.skip;

type Case = {
  name: string;
  files: Record<string, { contents: string; mode?: number }>;
  command: string;
  cwd?: string;
  /** Expected outcome class; digest equality is checked for "digest". */
  expect: "digest" | "null" | "error";
};

const CASES: Case[] = [
  {
    name: "bun test with a package.json script",
    files: {
      "bin/bun": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "bin/node": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "package.json": {
        contents: JSON.stringify({
          name: "t",
          scripts: { test: "node check.mjs" },
        }),
      },
      "check.mjs": { contents: "console.log(1)\n" },
    },
    command: "bun test",
    expect: "digest",
  },
  {
    name: "npm run build with a pre hook",
    files: {
      "bin/npm": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "bin/node": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "bin/vite": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "package.json": {
        contents: JSON.stringify({
          name: "t",
          scripts: { build: "vite build", prebuild: "node pre.js" },
        }),
      },
      "pre.js": { contents: "// pre\n" },
      "bun.lock": { contents: "v1\n" },
    },
    command: "npm run build",
    expect: "digest",
  },
  {
    name: "npx resolves the local runner",
    files: {
      "bin/npx": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "bin/vitest": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
    },
    command: "npx vitest run",
    expect: "digest",
  },
  {
    name: "a flag-named configuration is bound",
    files: {
      "bin/bun": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "bin/node": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "package.json": {
        contents: JSON.stringify({
          name: "t",
          scripts: { test: "node check.mjs" },
        }),
      },
      "check.mjs": { contents: "1\n" },
      "extra.toml": { contents: "a = 1\n" },
    },
    command: "bun test --config extra.toml",
    expect: "digest",
  },
  {
    name: "subdirectory cwd",
    files: {
      "bin/bun": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "bin/node": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "package.json": {
        contents: JSON.stringify({
          name: "t",
          scripts: { test: "node x.mjs" },
        }),
      },
      "x.mjs": { contents: "1\n" },
      "sub/keep.txt": { contents: "x" },
    },
    command: "bun test",
    cwd: "sub",
    expect: "digest",
  },
  {
    name: "unresolved executable is null, not an error",
    files: {
      "bin/bun": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
    },
    command: "definitely-not-installed-ak x",
    expect: "null",
  },
  {
    name: "a missing flag-named configuration is a refusal",
    files: {
      "bin/bun": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
    },
    command: "bun test --config missing.toml",
    expect: "error",
  },
  {
    name: "unreadable package.json is a refusal",
    files: {
      "bin/bun": { contents: "#!/bin/sh\nexit 0\n", mode: 0o755 },
      "package.json": { contents: "{not json" },
    },
    command: "bun test",
    expect: "error",
  },
];

async function rustOutcome(
  spec: Record<string, unknown>,
): Promise<{ kind: string; digest?: string }> {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-fp-diff-"));
  const specPath = join(directory, "spec.json");
  try {
    writeFileSync(specPath, JSON.stringify(spec));
    const proc = Bun.spawn(
      [
        "cargo",
        "run",
        "--manifest-path",
        join(root, "rust", "Cargo.toml"),
        "-p",
        "agentkit-toolpacks-coding",
        "--example",
        "fingerprint_cli",
        "--locked",
        "--offline",
        "--quiet",
        "--",
        specPath,
      ],
      { cwd: root, stdout: "pipe", stderr: "pipe" },
    );
    const [code, stdout, stderr] = await Promise.all([
      proc.exited,
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
    ]);
    const line = stdout.trim();
    expect(line === "" ? stderr : "", `cli exit ${code}`).not.toContain(
      "panicked",
    );
    if (line === "null") return { kind: "null" };
    if (line.startsWith("error:")) return { kind: "error" };
    expect(code, `${stdout}\n${stderr}`).toBe(0);
    return { kind: "digest", digest: line };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

async function tsOutcome(
  rootPath: string,
  cwdPath: string,
  command: string,
  environment: Record<string, string>,
): Promise<{ kind: string; digest?: string }> {
  try {
    const digest = await commandFingerprint(
      rootPath,
      cwdPath,
      command,
      environment,
    );
    return digest === null ? { kind: "null" } : { kind: "digest", digest };
  } catch {
    return { kind: "error" };
  }
}

describe("rust command fingerprint differential", () => {
  for (const testCase of CASES) {
    differential(
      `matches the TypeScript authority: ${testCase.name}`,
      async () => {
        const rootPath = realpathSync(
          mkdtempSync(join(tmpdir(), "ak-fp-diff-")),
        );
        try {
          for (const [name, file] of Object.entries(testCase.files)) {
            const path = join(rootPath, name);
            mkdirSync(dirname(path), { recursive: true });
            writeFileSync(path, file.contents);
            if (file.mode !== undefined) chmodSync(path, file.mode);
          }
          const cwdPath = join(rootPath, testCase.cwd ?? ".");
          const environment = { PATH: join(rootPath, "bin") };
          const expected = {
            root: rootPath,
            cwd: cwdPath,
            command: testCase.command,
            environment,
          };
          const [rust, ts] = await Promise.all([
            rustOutcome(expected),
            tsOutcome(rootPath, cwdPath, testCase.command, environment),
          ]);
          expect(ts.kind).toBe(testCase.expect);
          expect(rust.kind).toBe(testCase.expect);
          if (testCase.expect === "digest") {
            expect(rust.digest).toBe(ts.digest);
          }
        } finally {
          rmSync(rootPath, { recursive: true, force: true });
        }
      },
    );
  }

  differential(
    "a workspace-external direct configuration is refused by both",
    async () => {
      const outer = realpathSync(mkdtempSync(join(tmpdir(), "ak-fp-outer-")));
      const rootPath = realpathSync(mkdtempSync(join(tmpdir(), "ak-fp-diff-")));
      try {
        writeFileSync(join(outer, "evil.toml"), "bad = 1\n");
        mkdirSync(join(rootPath, "bin"), { recursive: true });
        writeFileSync(join(rootPath, "bin", "bun"), "#!/bin/sh\nexit 0\n");
        chmodSync(join(rootPath, "bin", "bun"), 0o755);
        const environment = { PATH: join(rootPath, "bin") };
        const command = `bun test --config ${join(outer, "evil.toml")}`;
        const rust = await rustOutcome({
          root: rootPath,
          cwd: rootPath,
          command,
          environment,
        });
        const ts = await tsOutcome(rootPath, rootPath, command, environment);
        expect(ts.kind).toBe("error");
        expect(rust.kind).toBe("error");
      } finally {
        rmSync(outer, { recursive: true, force: true });
        rmSync(rootPath, { recursive: true, force: true });
      }
    },
  );
});
