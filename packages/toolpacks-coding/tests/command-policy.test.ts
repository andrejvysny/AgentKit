import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiToolDefinition } from "@agentkit/contracts";
import type {
  StandingApprovalKey,
  ToolGuard,
  ToolGuardContext,
  ToolGuardVerdict,
} from "@agentkit/host";
import {
  type CodingMode,
  type CodingRole,
  classifyShellCommand,
  createCodingPermissionGuard,
  createCodingStandingApprovals,
  createHighRiskCommandGuard,
} from "../src/index.js";

const ELIGIBLE = [
  "bun test",
  "bun test packages/host --timeout 5000",
  "bun run test",
  "bun run lint",
  "npm run typecheck",
  "pnpm run check",
  "yarn run build",
  "npm run test:unit",
  "npm run lint:ci",
  "npm run format:check",
  "npm test",
  "pnpm test",
  "yarn test",
  "npx tsc --noEmit",
  "bunx tsc -p tsconfig.json",
  "tsc --noEmit",
  "npx vitest run src/a.test.ts",
  "vitest run",
  "npx jest --ci",
  "jest",
  "npx eslint src",
  "eslint .",
  "npx prettier --check .",
  "pytest -q tests/",
  "python -m pytest tests",
  "python3 -m unittest discover",
  "uv run pytest",
  "cargo test --workspace",
  "cargo check",
  "cargo build",
  "cargo build --release",
  "cargo clippy",
  "cargo fmt --check",
  "go test ./...",
  "go vet ./...",
  "make test",
  "make typecheck",
  "git status",
  "git diff HEAD",
  "git log --oneline -n 5",
  "git show HEAD",
];

const HIGH_RISK = [
  "rm -rf x",
  "rm x",
  "sudo bun test",
  "curl https://example.com",
  "bun run deploy",
  "npm run build:prod",
  "npm run db:migrate",
  "bun run release",
  "git push",
  "npx vite build --mode=production",
  "git fetch",
  "make deploy-docs",
  "git push --force",
  "git reset --hard",
  "git checkout main",
  "git commit -m x",
  "git stash",
  "git clean -fd",
  "npm publish",
  "bun publish",
  "cargo publish",
  "yarn login",
  "gh pr create",
  "docker run x",
  "find . -name x -delete",
  "find . -exec ls {} ;",
  "echo ok; rm -rf x",
  "bun test && curl x",
  "bun test | xargs echo",
  "echo $(rm x)",
  "bun test --no-verify",
  "bun test --force",
  "kill 1",
  "aws s3 ls",
  "chmod 777 x",
  "eval ls",
];

const UNKNOWN = [
  "ls",
  "cat README.md",
  "echo hi",
  "bun run dev",
  "bun run test extra",
  "npm run build --silent",
  "npm test -- --watch",
  "npm install",
  "bun test > out.txt",
  "bun test | tee out",
  "bun test; echo done",
  "bun test && echo ok",
  'bun test "a b"',
  "bun test 'a'",
  "bun test $HOME",
  "bun test `pwd`",
  "bun test *.ts",
  "bun test ~/x",
  "git diff HEAD~1",
  "bun test\nbun test",
  "bun test\tfoo",
  "bun test #comment",
  "NODE_ENV=test bun test",
  "FOO=1 pytest",
  "make",
  "make test lint",
  "node script.js",
  "python script.py",
  "npx prettier --write .",
  "",
  "   ",
];

describe("classifyShellCommand", () => {
  test.each(ELIGIBLE)("eligible: %s", (command) => {
    expect(classifyShellCommand(command)).toBe("eligible");
  });
  test.each(HIGH_RISK)("high_risk: %s", (command) => {
    expect(classifyShellCommand(command)).toBe("high_risk");
  });
  test.each(UNKNOWN)("unknown: %j", (command) => {
    expect(classifyShellCommand(command)).toBe("unknown");
  });
});

// --- keyFor ----------------------------------------------------------------

function def(name: string, capability = "process.exec"): AiToolDefinition {
  return {
    name,
    version: "1",
    effect: "write",
    capability,
    description: "d",
    inputSchema: { type: "object" },
  };
}

function ctxFor(
  args: unknown,
  overrides: Partial<ToolGuardContext> = {},
): ToolGuardContext {
  return {
    bindings: [],
    namespace: "coding",
    tool: def("run_command"),
    chatId: "chat-1",
    arguments: args,
    ...overrides,
  };
}

const ASK = [{ reason: "approval_required: run_command" }];

let root: string;
let outside: string;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "agentkit-cmdpol-"));
  outside = mkdtempSync(join(tmpdir(), "agentkit-cmdpol-out-"));
  mkdirSync(join(root, "pkg"));
  mkdirSync(join(root, "other"));
  symlinkSync(outside, join(root, "escape"));
});

afterEach(() => {
  rmSync(root, { recursive: true, force: true });
  rmSync(outside, { recursive: true, force: true });
});

function policy(identity = "id-1", context?: string, rootPath = root) {
  return createCodingStandingApprovals({
    resolve: async () => ({
      root: rootPath,
      identity,
      ...(context === undefined ? {} : { context }),
    }),
  });
}

async function keyOf(
  args: unknown,
  overrides: Partial<ToolGuardContext> = {},
  refusals = ASK,
  p = policy(),
): Promise<StandingApprovalKey | null> {
  return p.keyFor(ctxFor(args, overrides), refusals);
}

describe("createCodingStandingApprovals.keyFor", () => {
  test("keys an eligible command; the label names a non-root cwd", async () => {
    const here = await keyOf({ command: "bun test" });
    expect(here?.digest).toMatch(/^[0-9a-f]{64}$/);
    expect(here?.label).toBe("bun test");
    const nested = await keyOf({ command: "bun test", cwd: "pkg" });
    expect(nested?.label).toBe("bun test (in pkg)");
    // "." and the root spelled out are the same place.
    expect((await keyOf({ command: "bun test", cwd: "." }))?.digest).toBe(
      here?.digest,
    );
    expect((await keyOf({ command: "bun test", cwd: "pkg/.." }))?.digest).toBe(
      here?.digest,
    );
  });

  test("run_tests is covered the same way", async () => {
    const key = await policy().keyFor(
      ctxFor({ command: "bun test" }, { tool: def("run_tests") }),
      ASK,
    );
    expect(key).not.toBeNull();
    expect(key?.digest).not.toBe(
      (await keyOf({ command: "bun test" }))?.digest,
    );
  });

  test("any change to command, cwd, root, identity, context or chat changes the digest", async () => {
    const base = (await keyOf({ command: "bun test" }))?.digest;
    const variants = [
      await keyOf({ command: "bun test " }),
      await keyOf({ command: "bun test --bail" }),
      await keyOf({ command: "bun test", cwd: "pkg" }),
      await keyOf({ command: "bun test", cwd: "other" }),
      await keyOf({ command: "bun test" }, {}, ASK, policy("id-2")),
      await keyOf({ command: "bun test" }, {}, ASK, policy("id-1", "ctx")),
      await keyOf(
        { command: "bun test" },
        {},
        ASK,
        policy("id-1", undefined, join(root, "pkg")),
      ),
      await keyOf({ command: "bun test" }, { chatId: "chat-2" }),
    ];
    const digests = variants.map((k) => k?.digest);
    expect(digests.every((d) => d !== undefined && d !== base)).toBe(true);
    expect(new Set(digests).size).toBe(digests.length);
    expect((await keyOf({ command: "bun test" }))?.digest).toBe(base);
  });

  test("returns null for anything but plain approval_required refusals", async () => {
    for (const refusals of [
      [],
      [{ reason: "uncertain_tool_result: run_command was interrupted" }],
      [{ reason: "high_risk_command: run_command needs a fresh approval" }],
      [{ reason: "plan_approval_required: submit_plan" }],
      [
        ...ASK,
        { reason: "high_risk_command: run_command needs a fresh approval" },
      ],
      [...ASK, { reason: "something else" }],
    ])
      expect(await keyOf({ command: "bun test" }, {}, refusals)).toBeNull();
  });

  test("returns null for ineligible, malformed or uncovered calls", async () => {
    expect(await keyOf({ command: "bun run deploy" })).toBeNull();
    expect(await keyOf({ command: "ls" })).toBeNull();
    expect(await keyOf({ command: "" })).toBeNull();
    expect(await keyOf({ command: 1 })).toBeNull();
    expect(await keyOf({ command: "bun test", cwd: 5 })).toBeNull();
    expect(await keyOf(null)).toBeNull();
    expect(await keyOf(["bun test"])).toBeNull();
    expect(
      await keyOf({ command: "bun test" }, { chatId: undefined }),
    ).toBeNull();
    expect(
      await keyOf({ command: "bun test" }, { namespace: "other" }),
    ).toBeNull();
    expect(
      await keyOf(
        { command: "bun test" },
        { tool: def("write_file", "fs.write") },
      ),
    ).toBeNull();
    const noWorkspace = createCodingStandingApprovals({
      resolve: async () => null,
    });
    expect(
      await keyOf({ command: "bun test" }, {}, ASK, noWorkspace),
    ).toBeNull();
  });

  test("returns null when cwd leaves the root, is missing, or is a file", async () => {
    expect(await keyOf({ command: "bun test", cwd: "escape" })).toBeNull();
    expect(await keyOf({ command: "bun test", cwd: "escape/x" })).toBeNull();
    expect(await keyOf({ command: "bun test", cwd: ".." })).toBeNull();
    expect(await keyOf({ command: "bun test", cwd: "/etc" })).toBeNull();
    expect(await keyOf({ command: "bun test", cwd: "missing" })).toBeNull();
    expect(
      await keyOf(
        { command: "bun test" },
        {},
        ASK,
        policy("i", undefined, join(root, "gone")),
      ),
    ).toBeNull();
  });
});

// --- the high-risk guard, composed with the permission guard ----------------

const MODES: CodingMode[] = [
  "read_only",
  "ask",
  "auto_edit",
  "full_auto",
  "plan",
];

async function verdicts(
  mode: CodingMode,
  role: CodingRole,
  command: string,
  tool = "run_command",
): Promise<ToolGuardVerdict[]> {
  const guards: ToolGuard[] = [
    createCodingPermissionGuard({ mode, role }),
    createHighRiskCommandGuard(),
  ];
  const ctx = ctxFor({ command }, { tool: def(tool) });
  return Promise.all(
    guards.map((g) => g.canExecute!(ctx) as Promise<ToolGuardVerdict>),
  );
}

function reasons(list: ToolGuardVerdict[]): string[] {
  return list.flatMap((v) => (v.allowed ? [] : [v.reason]));
}

describe("createHighRiskCommandGuard", () => {
  test("allows everything that is not a high-risk shell call", async () => {
    const guard = createHighRiskCommandGuard();
    expect(await guard.canExecute!(ctxFor({ command: "bun test" }))).toEqual({
      allowed: true,
    });
    expect(await guard.canExecute!(ctxFor({ command: "ls" }))).toEqual({
      allowed: true,
    });
    expect(
      await guard.canExecute!(
        ctxFor({ command: "rm -rf x" }, { namespace: "other" }),
      ),
    ).toEqual({ allowed: true });
    expect(
      await guard.canExecute!(
        ctxFor(
          { command: "rm -rf x" },
          { tool: def("write_file", "fs.write") },
        ),
      ),
    ).toEqual({ allowed: true });
    expect(await guard.canExecute!(ctxFor({ nope: 1 }))).toEqual({
      allowed: true,
    });
  });

  test("a high-risk command is an approvable, non-standing refusal", async () => {
    const verdict = await createHighRiskCommandGuard().canExecute!(
      ctxFor({ command: "git push" }, { tool: def("run_tests") }),
    );
    expect(verdict).toEqual({
      allowed: false,
      reason: "high_risk_command: run_tests needs a fresh approval",
      approval: {},
    });
  });

  test("composed: ask/auto_edit get two approvable refusals, full_auto one, ceilings and plan stay final", async () => {
    for (const mode of ["ask", "auto_edit"] as const) {
      const r = reasons(await verdicts(mode, "implementer", "rm -rf x"));
      expect(r).toHaveLength(2);
      expect(r[0]).toStartWith("approval_required:");
      expect(r[1]).toStartWith("high_risk_command:");
    }
    const full = await verdicts("full_auto", "implementer", "rm -rf x");
    expect(reasons(full)).toEqual([
      "high_risk_command: run_command needs a fresh approval",
    ]);
    const finals = [
      ...(await verdicts("read_only", "implementer", "rm -rf x")),
      ...(await verdicts("plan", "implementer", "rm -rf x")),
      ...(await verdicts("full_auto", "supervisor", "rm -rf x")),
    ].filter((v) => !v.allowed && v.approval === undefined);
    expect(finals).toHaveLength(3);
    // An ordinary command in full_auto needs nothing at all.
    expect(
      reasons(await verdicts("full_auto", "implementer", "bun test")),
    ).toEqual([]);
    for (const mode of MODES) {
      const high = reasons(await verdicts(mode, "implementer", "rm -rf x"));
      // Whatever the mode says, a high-risk call is never left with only
      // the ordinary refusal that a standing approval could cover.
      expect(high.some((r) => r.startsWith("high_risk_command:"))).toBe(true);
    }
  });

  test("standing keys cannot cover a call the composed guards refused for risk", async () => {
    const p = policy();
    const risky = await verdicts("ask", "implementer", "bun test && rm -rf x");
    const refusals = reasons(risky).map((reason) => ({ reason }));
    expect(
      await p.keyFor(ctxFor({ command: "bun test && rm -rf x" }), refusals),
    ).toBeNull();
    const calm = await verdicts("ask", "implementer", "bun test");
    const calmRefusals = reasons(calm).map((reason) => ({ reason }));
    expect(calmRefusals).toHaveLength(1);
    expect(
      await p.keyFor(ctxFor({ command: "bun test" }), calmRefusals),
    ).not.toBeNull();
  });
});
