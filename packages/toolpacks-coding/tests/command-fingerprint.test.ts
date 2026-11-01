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
  for (const name of ["bun", "node", "npx", "bunx"])
    writeFileSync(join(root, "bin", name), "#!/bin/sh\nexit 0\n", {
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
    writeFileSync(join(root, "scripts", "changed.js"), "console.log(2);\n");
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

  test.each([
    "--preload ./first.ts --preload ./second.ts",
    "--preload=./first.ts --preload=./second.ts",
    "--preload ./first.ts --preload=./second.ts",
    "--config ./first.ts --config=./second.ts",
  ])("every repeated/attached direct input is bound: %s", async (flags) => {
    writeFileSync(join(root, "first.ts"), "export const first = 1;\n");
    writeFileSync(join(root, "second.ts"), "export const second = 1;\n");
    const first = await key(`bun test ${flags}`);
    expect(first).not.toBeNull();
    writeFileSync(join(root, "second.ts"), "export const second = 2;\n");
    const second = await key(`bun test ${flags}`);
    expect(second).not.toBeNull();
    expect(second?.digest).not.toBe(first?.digest);
    writeFileSync(join(root, "source.ts"), "export const unrelated = 3;\n");
    expect(await key(`bun test ${flags}`)).toEqual(second);
  });

  test.each(["./scripts/test", "'./scripts/test'", "./scripts/test --check"])(
    "extensionless literal package executable is bound: %s",
    async (script) => {
      writeFileSync(join(root, "scripts", "test"), "#!/bin/sh\nexit 0\n", {
        mode: 0o755,
      });
      writeFileSync(
        join(root, "package.json"),
        JSON.stringify({ scripts: { test: script } }),
      );
      const first = await key();
      expect(first).not.toBeNull();
      writeFileSync(join(root, "scripts", "test"), "#!/bin/sh\nexit 1\n");
      expect((await key())?.digest).not.toBe(first?.digest);
    },
  );

  test.each([
    "--preload ./missing.ts",
    "--preload=./missing.ts",
    "--preload",
    "--preload=",
    "--config=./missing.json",
    "--preload ./scripts",
    "--preload=external-package",
  ])(
    "unresolved supported direct input declines standing: %s",
    async (flags) => {
      expect(await key(`bun test ${flags}`)).toBeNull();
    },
  );

  test.each([
    "./scripts/missing",
    "node scripts/missing.js",
    "node --require ./missing scripts/test.js",
    "bun --preload=./missing scripts/test.js",
  ])("unresolved package entrypoint declines standing: %s", async (script) => {
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ scripts: { test: script } }),
    );
    expect(await key()).toBeNull();
  });

  test("direct input escaping workspace declines standing", async () => {
    symlinkSync("/bin/sh", join(root, "bootstrap.ts"));
    expect(await key("bun test --preload ./bootstrap.ts")).toBeNull();
  });

  test.each([
    "--preload ./scripts",
    "--preload=./scripts",
    "--config ./scripts",
    "--project ./scripts",
  ])(
    "non-TypeScript directory input cannot bind an unrelated tsconfig: %s",
    async (flags) => {
      writeFileSync(join(root, "scripts", "tsconfig.json"), "{}\n");
      expect(await key(`bun test ${flags}`)).toBeNull();
    },
  );

  test.each(["--project ./scripts", "-p ./scripts", "-p./scripts"])(
    "TypeScript project directory binds its tsconfig: %s",
    async (flags) => {
      writeFileSync(join(root, "bin", "tsc"), "#!/bin/sh\nexit 0\n", {
        mode: 0o755,
      });
      writeFileSync(join(root, "scripts", "tsconfig.json"), "{}\n");
      const first = await key(`tsc ${flags}`);
      expect(first).not.toBeNull();
      writeFileSync(
        join(root, "scripts", "tsconfig.json"),
        '{"strict":true}\n',
      );
      expect((await key(`tsc ${flags}`))?.digest).not.toBe(first?.digest);
    },
  );

  test("ordinary tsc argument cannot enable directory project semantics", async () => {
    writeFileSync(join(root, "scripts", "tsconfig.json"), "{}\n");
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({
        scripts: { test: "node scripts/test.js --project ./scripts tsc" },
      }),
    );
    expect(await key()).toBeNull();
  });

  test.each(["tsc", "MODE=test tsc", "env MODE=test tsc", "command tsc"])(
    "leading TypeScript runner retains directory semantics: %s",
    async (runner) => {
      writeFileSync(join(root, "bin", "tsc"), "#!/bin/sh\nexit 0\n", {
        mode: 0o755,
      });
      writeFileSync(join(root, "scripts", "tsconfig.json"), "{}\n");
      writeFileSync(
        join(root, "package.json"),
        JSON.stringify({ scripts: { test: `${runner} --project ./scripts` } }),
      );
      const first = await key();
      expect(first).not.toBeNull();
      writeFileSync(
        join(root, "scripts", "tsconfig.json"),
        '{"strict":true}\n',
      );
      expect((await key())?.digest).not.toBe(first?.digest);
    },
  );

  test.each([
    "node -r./bootstrap scripts/test.js",
    "node --require=./bootstrap scripts/test.js",
    "bun --preload=./bootstrap --preload ./second.ts scripts/test.js",
  ])(
    "package startup flags bind literal attached/repeated inputs: %s",
    async (script) => {
      writeFileSync(join(root, "bootstrap"), "// bootstrap 1\n");
      writeFileSync(join(root, "second.ts"), "export const second = 1;\n");
      writeFileSync(
        join(root, "package.json"),
        JSON.stringify({ scripts: { test: script } }),
      );
      const first = await key();
      expect(first).not.toBeNull();
      writeFileSync(join(root, "bootstrap"), "// bootstrap 2\n");
      const second = await key();
      expect(second).not.toBeNull();
      expect(second?.digest).not.toBe(first?.digest);
    },
  );

  test.each([
    "bun ./scripts/test",
    "node --require ./bootstrap.js ./scripts/test",
    "node -r./bootstrap.js ./scripts/test",
    "node --require=./bootstrap.js --import ./second.ts ./scripts/test",
    "bun --preload ./bootstrap.js ./scripts/test",
  ])(
    "extensionless interpreter entry after supported flags is bound: %s",
    async (script) => {
      writeFileSync(join(root, "scripts", "test"), "// direct entry 1\n");
      writeFileSync(join(root, "bootstrap.js"), "// bootstrap\n");
      writeFileSync(join(root, "second.ts"), "// import\n");
      writeFileSync(
        join(root, "package.json"),
        JSON.stringify({ scripts: { test: script } }),
      );
      const first = await key();
      expect(first).not.toBeNull();
      writeFileSync(join(root, "scripts", "test"), "// direct entry 2\n");
      const second = await key();
      expect(second).not.toBeNull();
      expect(second?.digest).not.toBe(first?.digest);
    },
  );

  test.each([
    "node --unsupported ./scripts/test",
    "node --require ./bootstrap.js ./scripts/missing",
    "bun ./scripts/missing",
  ])(
    "ambiguous or missing direct interpreter entry declines standing: %s",
    async (script) => {
      writeFileSync(join(root, "bootstrap.js"), "// bootstrap\n");
      writeFileSync(join(root, "scripts", "test"), "// direct entry\n");
      writeFileSync(
        join(root, "package.json"),
        JSON.stringify({ scripts: { test: script } }),
      );
      expect(await key()).toBeNull();
    },
  );

  test.each([
    "bun run ./scripts/test",
    "bun run test:unit",
    "bun build ./scripts/test",
  ])("nested Bun entry ambiguity declines standing: %s", async (script) => {
    writeFileSync(join(root, "scripts", "test"), "// direct entry\n");
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ scripts: { test: script } }),
    );

    expect(await key()).toBeNull();
  });

  test("package Bun test keeps unrelated discovered source edits covered", async () => {
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ scripts: { test: "bun test" } }),
    );
    const first = await key();
    expect(first).not.toBeNull();
    writeFileSync(
      join(root, "discovered.test.ts"),
      "export const ordinary = 1;\n",
    );
    expect(await key()).toEqual(first);
    writeFileSync(
      join(root, "discovered.test.ts"),
      "export const ordinary = 2;\n",
    );
    expect(await key()).toEqual(first);
  });

  test.each([
    "cd scripts && ./test",
    "env -i ./scripts/test",
    "node --require $BOOTSTRAP scripts/test.js",
    "npx node ./scripts/test",
    "bunx node ./scripts/test",
  ])("unsupported command resolution declines standing: %s", async (script) => {
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ scripts: { test: script } }),
    );
    expect(await key()).toBeNull();
  });
});

describe("package script executable binding", () => {
  test("PATH assignment declines standing approval", async () => {
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ scripts: { test: "PATH=./tools vitest run" } }),
    );
    writeFileSync(join(root, "bin", "vitest"), "#!/bin/sh\nexit 0\n", {
      mode: 0o755,
    });
    expect(await key()).toBeNull();
  });

  test.each(["npx", "bunx"])(
    "nested %s declines standing approval",
    async (runner) => {
      writeFileSync(
        join(root, "package.json"),
        JSON.stringify({ scripts: { test: `${runner} tsc --noEmit` } }),
      );
      writeFileSync(join(root, "bin", "tsc"), "#!/bin/sh\nexit 0\n", {
        mode: 0o755,
      });
      expect(await key()).toBeNull();
      writeFileSync(join(root, "bin", "tsc"), "#!/bin/sh\nexit 1\n", {
        mode: 0o755,
      });
      expect(await key()).toBeNull();
    },
  );
  function setup(scripts: Record<string, string> = { test: "vitest run" }) {
    writeFileSync(join(root, "bin", "npm"), "#!/bin/sh\nexit 0\n", {
      mode: 0o755,
    });
    mkdirSync(join(root, "node_modules", "vitest"), { recursive: true });
    mkdirSync(join(root, "node_modules", ".bin"), { recursive: true });
    const target = join(root, "node_modules", "vitest", "vitest.mjs");
    writeFileSync(target, "// v1\n", { mode: 0o755 });
    symlinkSync(target, join(root, "node_modules", ".bin", "vitest"));
    writeFileSync(join(root, "package.json"), JSON.stringify({ scripts }));
    return target;
  }

  test.each(["npm run test", "npm test"])(
    "executable bytes change invalidates: %s",
    async (command) => {
      const target = setup();
      const first = await key(command);
      expect(first).not.toBeNull();
      expect(await key(command)).toEqual(first);
      writeFileSync(target, "// v2 longer\n", { mode: 0o755 });
      const second = await key(command);
      expect(second).not.toBeNull();
      expect(second?.digest).not.toBe(first?.digest);
    },
  );

  test("retargeted .bin symlink invalidates", async () => {
    setup();
    const first = await key("npm run test");
    const other = join(root, "node_modules", "vitest", "other.mjs");
    writeFileSync(other, "// v1\n", { mode: 0o755 });
    const link = join(root, "node_modules", ".bin", "vitest");
    rmSync(link);
    symlinkSync(other, link);
    const second = await key("npm run test");
    expect(second).not.toBeNull();
    expect(second?.digest).not.toBe(first?.digest);
  });

  test("ancestor .bin is used when package dir has none", async () => {
    const target = setup({});
    mkdirSync(join(root, "pkg"));
    writeFileSync(
      join(root, "pkg", "package.json"),
      JSON.stringify({ scripts: { test: "vitest run" } }),
    );
    const first = await key("npm run test", "pkg");
    expect(first).not.toBeNull();
    writeFileSync(target, "// v2 longer\n", { mode: 0o755 });
    const second = await key("npm run test", "pkg");
    expect(second).not.toBeNull();
    expect(second?.digest).not.toBe(first?.digest);
  });

  test("unresolvable executable declines standing", async () => {
    setup();
    rmSync(join(root, "node_modules", ".bin", "vitest"));
    expect(await key("npm run test")).toBeNull();
  });

  test("pre and post script executables are bound", async () => {
    const target = setup({
      test: "node scripts/test.js",
      pretest: "vitest run",
      posttest: "vitest run",
    });
    const first = await key("npm run test");
    expect(first).not.toBeNull();
    writeFileSync(target, "// v2 longer\n", { mode: 0o755 });
    expect((await key("npm run test"))?.digest).not.toBe(first?.digest);
    rmSync(join(root, "node_modules", ".bin", "vitest"));
    expect(await key("npm run test")).toBeNull();
  });

  test("non-script commands keep a stable fingerprint", async () => {
    const first = await key("bun test");
    expect(first).not.toBeNull();
    expect(await key("bun test")).toEqual(first);
  });
});

describe("NODE_OPTIONS startup selector binding (external review P1-D)", () => {
  function selectorTree(script: string) {
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ scripts: { test: script } }),
    );
    writeFileSync(join(root, "scripts", "cli.js"), "console.log('cli');\n");
    writeFileSync(join(root, "bootstrap.cjs"), "module.exports = 'v1';\n");
  }

  test.each([
    ["inherited", "node scripts/cli.js", "--require=./bootstrap.cjs"],
    [
      "assignment",
      "NODE_OPTIONS=--require=./bootstrap.cjs node scripts/cli.js",
      undefined,
    ],
    [
      "env wrapper",
      "NODE_OPTIONS=--require=./bootstrap.cjs env node scripts/cli.js",
      undefined,
    ],
    [
      "explicit CLI control",
      "node --require ./bootstrap.cjs scripts/cli.js",
      undefined,
    ],
  ] as [string, string, string | undefined][])(
    "%s: editing the selected startup file with a constant selector invalidates",
    async (_label, script, inherited) => {
      selectorTree(script);
      if (inherited !== undefined) environment.NODE_OPTIONS = inherited;
      else delete environment.NODE_OPTIONS;
      const first = await key("bun test");
      expect(first).not.toBeNull();
      expect(await key("bun test")).toEqual(first);
      writeFileSync(
        join(root, "bootstrap.cjs"),
        "module.exports = 'MUTATED';\n",
      );
      const mutated = await key("bun test");
      expect(mutated).not.toBeNull();
      expect(mutated?.digest).not.toBe(first?.digest);
      writeFileSync(join(root, "bootstrap.cjs"), "module.exports = 'v1';\n");
      expect(await key("bun test")).toEqual(first);
    },
  );

  test("assignment and inherited values are both bound", async () => {
    selectorTree("NODE_OPTIONS=--require=./assigned.cjs node scripts/cli.js");
    writeFileSync(join(root, "assigned.cjs"), "1;\n");
    writeFileSync(join(root, "inherited.cjs"), "1;\n");
    writeFileSync(join(root, "unselected.cjs"), "1;\n");
    environment.NODE_OPTIONS = "--require=./inherited.cjs";
    const first = await key("bun test");
    expect(first).not.toBeNull();
    writeFileSync(join(root, "inherited.cjs"), "MUTATED;\n");
    expect((await key("bun test"))?.digest).not.toBe(first?.digest);
    writeFileSync(join(root, "inherited.cjs"), "1;\n");
    writeFileSync(join(root, "assigned.cjs"), "MUTATED;\n");
    expect((await key("bun test"))?.digest).not.toBe(first?.digest);
    writeFileSync(join(root, "assigned.cjs"), "1;\n");
    writeFileSync(join(root, "unselected.cjs"), "MUTATED;\n");
    expect(await key("bun test")).toEqual(first);
  });

  test.each([
    ["absent selector file", "node scripts/cli.js", "--require=./never-ak.cjs"],
    ["empty equals target", "node scripts/cli.js", "--require="],
    ["dangling selector", "node scripts/cli.js", "--require"],
    [
      "flag as selector value",
      "node scripts/cli.js",
      "--require --require=./bootstrap.cjs",
    ],
    ["bare module specifier", "node scripts/cli.js", "--require=bootstrap.cjs"],
    [
      "directory as selector target",
      "node scripts/cli.js",
      "--require=./cfgdir",
    ],
    ["unsupported loader selector", "node scripts/cli.js", "--loader=./x.mjs"],
    [
      "unsupported experimental-loader",
      "node scripts/cli.js",
      "--experimental-loader=./x.mjs",
    ],
    [
      "CLI-only -r in NODE_OPTIONS",
      "node scripts/cli.js",
      "-r ./bootstrap.cjs",
    ],
    [
      "abbreviated --require prefix",
      "node scripts/cli.js",
      "--requir=./bootstrap.cjs",
    ],
    [
      "abbreviated --import prefix",
      "node scripts/cli.js",
      "--impor=./bootstrap.cjs",
    ],
    [
      "nonliteral selector value",
      "node scripts/cli.js",
      "--require=./$(evil).cjs",
    ],
    [
      "mid-command reassignment",
      "node scripts/cli.js NODE_OPTIONS=--require=./evil.cjs",
      undefined,
    ],
    [
      "duplicate assignment before the entry",
      "NODE_OPTIONS=--require=./bootstrap.cjs NODE_OPTIONS=--import=./bootstrap.cjs node scripts/cli.js",
      undefined,
    ],
    [
      "env wrapper option flags",
      "env -i NODE_OPTIONS=--require=./bootstrap.cjs node scripts/cli.js",
      undefined,
    ],
  ] as [string, string, string | undefined][])(
    "%s: declines standing approval instead of authorizing an unbound preload",
    async (_label, script, inherited) => {
      selectorTree(script);
      mkdirSync(join(root, "cfgdir"), { recursive: true });
      if (inherited !== undefined) environment.NODE_OPTIONS = inherited;
      else delete environment.NODE_OPTIONS;
      expect(await key("bun test")).toBeNull();
    },
  );

  test("unreadable selector target declines standing approval", async () => {
    selectorTree("node scripts/cli.js");
    environment.NODE_OPTIONS = "--require=./bootstrap.cjs";
    expect(await key("bun test")).not.toBeNull();
    chmodSync(join(root, "bootstrap.cjs"), 0o000);
    try {
      // Running as root can still read mode-000 files; only assert refusal
      // when the read genuinely fails (matches the Rust gate).
      const readable = await Bun.file(join(root, "bootstrap.cjs"))
        .text()
        .then(() => true)
        .catch(() => false);
      if (readable) return;
      expect(await key("bun test")).toBeNull();
    } finally {
      chmodSync(join(root, "bootstrap.cjs"), 0o644);
    }
  });
});
