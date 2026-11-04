/**
 * Permanent pins for the composition-ownership gate
 * (`scripts/check-composition-ownership.mjs`, review 2026-11-01 §6.6): the
 * gate must PASS the current tree and REJECT the regressions it exists to
 * catch — a reintroduced TS execution engine, a TS coding pack, a backend
 * value import outside the allowlist — while accepting bindings, public
 * types, and the documented allowlist.
 */
import { expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  HARD_BANNED,
  scanSource,
  scanTree,
} from "../check-composition-ownership.mjs";

const root = fileURLToPath(new URL("../../", import.meta.url));

it("passes the current tree with a non-trivial scan", () => {
  const { findings, files } = scanTree();
  expect(findings).toEqual([]);
  // A silently emptied scan would also "pass" — pin the coverage floor.
  expect(files).toBeGreaterThanOrEqual(17);
});

it("rejects the frozen TS execution engines as value imports", () => {
  const violations = scanSource(
    "examples/desktop-host/src/regression.ts",
    [
      `import { TurnRunner } from "@agentkit/host";`,
      `import { TaskService, type Logger } from "@agentkit/host";`,
      `import { createCodingToolPack } from "@agentkit/toolpacks-coding";`,
      `export { SingleProcessTaskRunner } from "agentkit/runner-local";`,
    ].join("\n"),
  );
  const text = JSON.stringify(violations);
  for (const symbol of ["TurnRunner", "TaskService", "createCodingToolPack"]) {
    expect(text).toInclude(symbol);
  }
  // A type-only re-export of the same package stays legal (public types).
  const clean = scanSource(
    "examples/desktop-host/src/regression.ts",
    `export type { Logger } from "@agentkit/host";`,
  );
  expect(clean).toEqual([]);
});

it("rejects a backend value import outside the per-file allowlist", () => {
  // `defaultProfileRegistry` is allowlisted ONLY for the coding-agent CLI
  // files; elsewhere it is still a backend value import.
  const violations = scanSource(
    "examples/desktop-host/src/wiring.ts",
    `import { defaultProfileRegistry } from "@agentkit/host";`,
  );
  expect(violations.length).toBe(1);
  expect(violations[0]?.violation).toInclude("not allowlisted");
  // Same symbol in the allowlisted file is clean.
  const allowed = scanSource(
    "examples/coding-agent/src/app.ts",
    `import { defaultProfileRegistry, readSessionDescriptor } from "@agentkit/host";`,
  );
  expect(allowed).toEqual([]);
});

it("rejects namespace and require imports of backend packages", () => {
  const ns = scanSource(
    "examples/coding-agent/src/sneaky.ts",
    `import * as host from "@agentkit/host";`,
  );
  expect(ns.length).toBe(1);
  expect(ns[0]?.violation).toInclude("whole-package");
  const req = scanSource(
    "scripts/qualification/sneaky.mjs",
    `const { TurnRunner } = require("@agentkit/host");`,
  );
  expect(req.length).toBe(1);
});

it("hard-bans are effective inside allowlisted files", () => {
  // app.ts is allowlisted for two host symbols; the banned engine symbols
  // must still fail there.
  const violations = scanSource(
    "examples/coding-agent/src/app.ts",
    `import { defaultProfileRegistry, TurnRunner } from "@agentkit/host";`,
  );
  expect(violations.length).toBe(1);
  expect(violations[0]?.violation).toInclude("hard-banned");
  expect(HARD_BANNED.has("TurnRunner")).toBe(true);
});

it("the CLI gate exits 0 on the clean tree and reports via stdout", () => {
  const result = spawnSync(
    "node",
    ["scripts/check-composition-ownership.mjs"],
    {
      cwd: root,
      encoding: "utf8",
      timeout: 60_000,
    },
  );
  expect(result.status).toBe(0);
  expect(result.stdout).toInclude("0 backend-implementation imports");
});
