import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  foundationExclusions,
  projectFoundation,
  sha256,
} from "../release/foundation.mjs";

import { sourceFixture } from "./snapshot-fixture";

const root = fileURLToPath(new URL("../../", import.meta.url));
const integrated = existsSync(
  join(root, "packages/core/src/providers/responses.ts"),
);
let scratch: string;
let fixtureRoot: string;

function snapshot(name: string, release: string): string {
  const directory = join(scratch, name);
  const version = release === "foundation" ? "0.6.0" : "0.7.0";
  const result = spawnSync(
    "node",
    ["scripts/snapshot-release.mjs", directory, version, release],
    {
      cwd: fixtureRoot,
      encoding: "utf8",
    },
  );
  if (result.status !== 0) throw new Error(result.stderr);
  return directory;
}

function read(directory: string, path: string): string {
  return readFileSync(join(directory, path), "utf8");
}

function assertDurableExecutionPreserved(directory: string): void {
  for (const path of [
    "packages/host/src/ports/tool-execution-store.ts",
    "packages/host/src/turn/durable-execution.ts",
    "packages/host/src/turn/registry-staging.ts",
    "packages/host/src/turn/guarded-tool.ts",
    "packages/host/src/turn/admission-protocol.ts",
    "packages/host/src/turn/side-effects.ts",
    "packages/host/src/sessions/approval-denial.ts",
    "packages/host/src/sessions/human-answer.ts",
    "packages/contracts/src/run-events.ts",
    "packages/host/src/turn/approval-resume.ts",
    "packages/adapters-memory/src/memory-tool-execution-store.ts",
    "packages/adapters-sqlite/src/sqlite/tool-execution-store.ts",
    "packages/adapters-sqlite/src/sqlite/migrations.ts",
    "packages/core/src/runs/bounded-provider.ts",
    "packages/host/src/context/compactor.ts",
    "packages/toolpacks-coding/src/command-fingerprint.ts",
    "packages/toolpacks-coding/src/command-risk.ts",
    "packages/toolpacks-coding/src/command-script-inputs.ts",
    "packages/toolpacks-coding/src/context.ts",
    "packages/toolpacks-coding/src/coordination-claim.ts",
    "packages/toolpacks-coding/src/process-identity.ts",
    "packages/toolpacks-coding/src/process-teardown.ts",
    "packages/toolpacks-coding/src/process-owned.ts",
  ])
    expect(read(directory, path)).toBe(read(fixtureRoot, path));
  expect(
    read(directory, "packages/host/src/ports/assistant-store.ts"),
  ).toContain(
    'import type { ToolExecutionStore } from "./tool-execution-store.js"',
  );
  expect(
    read(directory, "packages/host/src/ports/assistant-store.ts"),
  ).toContain("toolExecutions?: ToolExecutionStore");
  const sqlite = read(
    directory,
    "packages/adapters-sqlite/src/shared-assistant-store.ts",
  );
  expect(sqlite).toContain("ensureToolExecutionTables(db)");
  expect(sqlite).toContain("new SqliteToolExecutionStore");
  expect(
    read(directory, "packages/adapters-memory/src/memory-assistant-store.ts"),
  ).toContain("new MemoryToolExecutionStore");
  expect(
    read(directory, "packages/adapters-memory/src/memory-assistant-store.ts"),
  ).toContain("this.toolExecutions.pruneDeletedRuns()");
  expect(
    read(directory, "packages/adapters-memory/src/memory-assistant-store.ts"),
  ).toContain("this.onDeleteTasks?.()");
  expect(read(directory, "packages/host/src/turn/turn-runner.ts")).toContain(
    "durable:",
  );
  expect(
    read(directory, "packages/host/src/turn/admission-protocol.ts"),
  ).toContain("new WeakSet<AiTool>()");
  expect(read(directory, "packages/contracts/src/run-events.ts")).toContain(
    "admissionProtocol: Type.Optional(",
  );
  expect(read(directory, "packages/host/src/turn/turn-runner.ts")).toContain(
    "event = stampAdmissionProtocol(",
  );
  expect(read(directory, "packages/host/src/turn/side-effects.ts")).toContain(
    'event.data.admissionProtocol === "fenced-v1"',
  );
  expect(
    read(directory, "packages/host/src/turn/history-assembly.ts"),
  ).toContain("content: await providerHumanAnswerContent(deps.store, record)");
  expect(
    read(directory, "packages/host/src/sessions/approval-denial.ts"),
  ).toContain('priorExecutionOutcome: "unknown"');
  expect(
    read(directory, "packages/host/src/sessions/human-answer.ts"),
  ).toContain("approvalDenialEnvelope(request, approval.note)");
  const context = read(
    directory,
    "packages/host/src/context/bounded-context-client.ts",
  );
  expect(context).toContain("messageTokens > budgetTokens");
  expect(context).toContain('type: "run.context.admitted"');
  expect(context).toContain("maxOutputTokens: admission.maxOutputTokens");
  expect(read(directory, "packages/core/src/providers/client.ts")).toContain(
    "readonly maxOutputTokens?: number",
  );
  expect(read(directory, "packages/host/src/turn/turn-runner.ts")).toContain(
    "const client = await boundedContextClient(input.client",
  );
}

describe.skipIf(!integrated)("current-source foundation projection", () => {
  beforeAll(() => {
    scratch = mkdtempSync(join(tmpdir(), "agentkit-snapshot-test-"));
    fixtureRoot = sourceFixture(root, join(scratch, "source"));
  });
  afterAll(() => {
    rmSync(scratch, { recursive: true, force: true });
  });

  it("reproduces independently and preserves current generic source", () => {
    const first = snapshot("foundation-a", "foundation");
    const second = snapshot("foundation-b", "foundation");
    expect(read(first, "snapshot-provenance.json")).toBe(
      read(second, "snapshot-provenance.json"),
    );
    assertDurableExecutionPreserved(first);
    const reasoning = read(
      first,
      "packages/core/tests/reasoning-effort.test.ts",
    );
    expect(reasoning).toContain('describe("chat completions reasoning_effort"');
    expect(reasoning).toContain("drops it once on a 400 naming it and warns");
    expect(reasoning).not.toContain("responses-helpers");
    expect(reasoning).not.toContain('describe("Responses reasoning effort');
    expect(
      read(first, "packages/host/src/context/bounded-context-client.ts"),
    ).not.toContain("client.protocol");
    expect(
      read(first, "packages/host/src/context/bounded-context-client.ts"),
    ).not.toContain("request.runContent");
    for (const path of foundationExclusions)
      expect(existsSync(join(first, path))).toBe(false);
    for (const path of [
      "packages/core/src/providers/sse.ts",
      "packages/core/src/providers/openai-compatible.ts",
      "packages/client/src/sse.ts",
      "packages/client/src/stream.ts",
      "packages/transport-http/src/sse.ts",
      "packages/adapters-sqlite/src/sqlite/open.ts",
      "packages/adapters-sqlite/src/schema.ts",
      "packages/adapters-sqlite/src/sqlite/migrations.ts",
      "packages/adapters-sqlite/src/provider-continuation-schema.ts",
      "packages/adapters-sqlite/src/message-archive-schema.ts",
      "packages/adapters-sqlite/tests/archive-durability.test.ts",
      "scripts/sqlite-durability.mjs",
      "scripts/qualification/sqlite-fixture.mjs",
      "packages/host/src/prompts/run-content.ts",
      "packages/host/src/prompts/template-service.ts",
      "packages/host/src/skills/skill-service.ts",
      "packages/host/tests/skills-templates.test.ts",
      "packages/mcp-client/src/connections.ts",
      "packages/adapters-sqlite/src/shared-prompt-template-store.ts",
      "packages/runner-local/src/file-skill-source.ts",
      "examples/desktop-host/node-host.mjs",
    ])
      expect(read(first, path)).toBe(read(fixtureRoot, path));
    expect(read(first, "packages/host/src/turn/projection.ts")).toContain(
      "if (!(key in snapshot)) delete current[key]",
    );
    expect(
      read(first, "packages/adapters-sqlite/src/sqlite/migrations.ts"),
    ).toContain("unsupportedVersion(version, target)");
    expect(read(first, "packages/adapters-sqlite/src/schema.ts")).toContain(
      "SCHEMA_VERSION = 10",
    );
    expect(read(first, "packages/agentkit/package.json")).toContain(
      '"version": "0.6.0"',
    );
    const provenance = JSON.parse(read(first, "snapshot-provenance.json"));
    expect(provenance.patches).toHaveLength(6);
    const nativeFixtures = JSON.parse(
      read(first, "rust/fixtures/manifest.json"),
    );
    expect(nativeFixtures.contractVersion).toBe("0.6.0");
    const foundationSchemas = JSON.parse(
      read(first, "rust/fixtures/schemas.json"),
    );
    expect(nativeFixtures.schemaCount).toBe(
      Object.keys(foundationSchemas).length,
    );
    expect(foundationSchemas.AiProviderContinuationSchema).toBeUndefined();
    expect(read(first, "rust/crates/contracts/src/routes.rs")).toContain(
      'CONTRACT_VERSION: &str = "0.6.0"',
    );
    for (const path of [
      "rust/fixtures/schemas.json",
      "rust/crates/contracts/src/schema_bundle.json",
      "rust/crates/host/src/lib.rs",
    ])
      expect(
        provenance.changedFiles.some(
          (file: { path: string }) => file.path === path,
        ),
      ).toBe(true);
    for (const patch of provenance.patches)
      expect(patch.sha256).toBe(sha256(read(first, patch.path)));
    expect(
      provenance.excluded.every((file: { sha256: string }) =>
        /^[a-f0-9]{64}$/.test(file.sha256),
      ),
    ).toBe(true);
    expect(
      provenance.originalFiles.some(
        (file: { path: string }) =>
          file.path === "packages/core/src/providers/responses.ts",
      ),
    ).toBe(true);
  });

  it("fails closed on changed integration before removing excluded files", () => {
    const directory = snapshot("drift", "responses");
    const path = "packages/core/src/providers/client.ts";
    writeFileSync(
      join(directory, path),
      read(directory, path).replace(
        "continuationRequired?: boolean",
        "continuationRequired?: string",
      ),
    );
    expect(() => projectFoundation(directory)).toThrow(
      "Foundation projection drift",
    );
    expect(
      existsSync(join(directory, "packages/core/src/providers/responses.ts")),
    ).toBe(true);
    expect(read(directory, "packages/adapters-sqlite/src/schema.ts")).toContain(
      "SCHEMA_VERSION = 10",
    );
  });

  it("applies captured patches rather than reading mutable checkout patches", () => {
    const directory = snapshot("captured-patches", "responses");
    const path = "scripts/release/foundation/sqlite.patch";
    writeFileSync(
      join(directory, path),
      read(directory, path).replace(
        "type ProviderContinuationStore,",
        "type MissingContinuationStore,",
      ),
    );
    expect(() => projectFoundation(directory)).toThrow(
      "Foundation projection drift",
    );
  });

  it("retains Responses source and schema 10 in separate candidate", () => {
    const directory = snapshot("responses", "responses");
    assertDurableExecutionPreserved(directory);
    expect(
      read(directory, "packages/host/src/context/bounded-context-client.ts"),
    ).toBe(
      read(fixtureRoot, "packages/host/src/context/bounded-context-client.ts"),
    );
    expect(read(directory, "packages/core/src/providers/responses.ts")).toBe(
      read(fixtureRoot, "packages/core/src/providers/responses.ts"),
    );
    expect(read(directory, "packages/adapters-sqlite/src/schema.ts")).toContain(
      "SCHEMA_VERSION = 10",
    );
    const provenance = JSON.parse(read(directory, "snapshot-provenance.json"));
    expect(provenance.patches).toEqual([]);
    expect(provenance.excluded).toEqual([]);
  });
});
