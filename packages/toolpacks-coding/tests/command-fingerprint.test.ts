import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createCodingStandingApprovals } from "../src/command-policy.js";

let root: string;
let environment: NodeJS.ProcessEnv;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "agentkit-command-fingerprint-"));
  mkdirSync(join(root, "bin"));
  mkdirSync(join(root, "scripts"));
  writeFileSync(join(root, "bin", "bun"), "#!/bin/sh\nexit 0\n", {
    mode: 0o755,
  });
  writeFileSync(
    join(root, "package.json"),
    JSON.stringify({ scripts: { test: "node scripts/test.js" } }),
  );
  writeFileSync(join(root, "scripts", "test.js"), "console.log(1);\n");
  environment = { PATH: join(root, "bin"), HOME: root, NODE_ENV: "test" };
});
afterEach(() => rmSync(root, { recursive: true, force: true }));

async function key(command = "bun run test", cwd?: string) {
  return createCodingStandingApprovals({
    resolve: async () => ({ root, identity: "sandbox-v1", environment }),
  }).keyFor(
    {
      bindings: [],
      namespace: "coding",
      chatId: "s1",
      arguments: { command, ...(cwd === undefined ? {} : { cwd }) },
      tool: {
        name: "run_command",
        version: "1",
        effect: "write",
        capability: "process.exec",
        description: "d",
        inputSchema: {},
      },
    },
    [{ reason: "approval_required: run_command" }],
  );
}

describe("bounded standing approval fingerprint", () => {
  test("unchanged covered context reuses key; ordinary source edits remain covered", async () => {
    const first = await key();
    expect(first).not.toBeNull();
    expect(await key()).toEqual(first);
    writeFileSync(join(root, "source.ts"), "export const value = 2;\n");
    expect(await key()).toEqual(first);
    writeFileSync(join(root, "source.ts"), "export const value = 3;\n");
    expect(await key()).toEqual(first);
  });

  test.each([
    "package.json",
    "bun.lock",
    "bunfig.toml",
    "vitest.config.ts",
    "tsconfig.json",
    "scripts/test.js",
  ])("changed bound input invalidates: %s", async (path) => {
    const first = await key();
    writeFileSync(
      join(root, path),
      path === "package.json"
        ? JSON.stringify({ scripts: { test: "node scripts/changed.js" } })
        : "changed\n",
    );
    const changed = await key();
    expect(changed).not.toBeNull();
    expect(changed?.digest).not.toBe(first?.digest);
    expect(await key()).toEqual(changed);
  });

  test("executable replacement, permission, PATH and execution environment changes invalidate", async () => {
    const first = await key();
    writeFileSync(join(root, "bin", "bun"), "#!/bin/sh\nexit 1\n");
    const changed = await key();
    expect(changed?.digest).not.toBe(first?.digest);
    environment.NODE_OPTIONS = "--no-warnings";
    const withOptions = await key();
    expect(withOptions?.digest).not.toBe(changed?.digest);
    environment.PATH += ":/usr/bin";
    expect((await key())?.digest).not.toBe(withOptions?.digest);
    chmodSync(join(root, "bin", "bun"), 0o644);
    expect(await key()).toBeNull();
  });

  test("resolved executable symlink identity is bound", async () => {
    const first = await key();
    writeFileSync(join(root, "bin", "alternate"), "#!/bin/sh\nexit 0\n", {
      mode: 0o755,
    });
    rmSync(join(root, "bin", "bun"));
    symlinkSync("alternate", join(root, "bin", "bun"));
    expect((await key())?.digest).not.toBe(first?.digest);
  });

  test("nested cwd binds relevant ancestor metadata", async () => {
    mkdirSync(join(root, "pkg"));
    const first = await key("bun test", "pkg");
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ dependencies: { example: "2" } }),
    );
    expect((await key("bun test", "pkg"))?.digest).not.toBe(first?.digest);
  });

  test("missing executable and oversized metadata offer no grant", async () => {
    environment.PATH = join(root, "missing");
    expect(await key()).toBeNull();
    environment.PATH = join(root, "bin");
    writeFileSync(join(root, "bun.lock"), Buffer.alloc(8 * 1024 * 1024 + 1));
    expect(await key()).toBeNull();
  });

  test("secret/unrelated environment changes do not alter covered context", async () => {
    const first = await key();
    environment.API_TOKEN = "credential-must-not-enter-fingerprint";
    environment.SSH_AUTH_SOCK = "/unrelated/agent.sock";
    environment.UNRELATED_VALUE = "unrelated";
    expect(await key()).toEqual(first);
  });

  test("explicit configuration and shell startup file changes invalidate", async () => {
    writeFileSync(join(root, "custom.json"), "{}\n");
    const first = await key("bun test --config custom.json");
    writeFileSync(join(root, "custom.json"), '{"changed":true}\n');
    expect((await key("bun test --config custom.json"))?.digest).not.toBe(
      first?.digest,
    );
    environment.BASH_ENV = join(root, "startup.sh");
    writeFileSync(environment.BASH_ENV, "# initial\n");
    const startup = await key();
    writeFileSync(environment.BASH_ENV, "# changed\n");
    expect((await key())?.digest).not.toBe(startup?.digest);
  });

  test("npx local CLI resolution is bound, unresolved runner offers no grant", async () => {
    writeFileSync(join(root, "bin", "npx"), "#!/bin/sh\nexit 0\n", {
      mode: 0o755,
    });
    expect(await key("npx tsc --noEmit")).toBeNull();
    mkdirSync(join(root, "node_modules", ".bin"), { recursive: true });
    const cli = join(root, "node_modules", ".bin", "tsc");
    writeFileSync(cli, "#!/bin/sh\nexit 0\n", { mode: 0o755 });
    const first = await key("npx tsc --noEmit");
    expect(first).not.toBeNull();
    writeFileSync(cli, "#!/bin/sh\nexit 1\n");
    expect((await key("npx tsc --noEmit"))?.digest).not.toBe(first?.digest);
  });
});
