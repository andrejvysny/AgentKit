import { expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../", import.meta.url));
const declaration = new URL(
  "../../packages/core/dist/index.d.ts",
  import.meta.url,
);
const enabled = process.env.AGENTKIT_CHECK_API_GATE === "1";

it.skipIf(!enabled)(
  "rejects a change to inherited public members",
  () => {
    const inherited = new URL(
      "../../packages/adapters-sqlite/dist/shared-assistant-store.d.ts",
      import.meta.url,
    );
    if (!existsSync(inherited)) throw new Error("Run bun run build first");
    const before = readFileSync(inherited, "utf8");
    const after = before.replace(
      "get database(): Database;",
      "get database(): never;",
    );
    expect(after).not.toBe(before);
    const report = new URL(
      "../../docs/rust/api/adapters-sqlite.api.md",
      import.meta.url,
    );
    const accepted = readFileSync(report, "utf8");
    try {
      writeFileSync(inherited, after);
      const result = spawnSync(
        "node",
        ["scripts/check-public-api.mjs", "--entry=adapters-sqlite"],
        {
          cwd: root,
          encoding: "utf8",
          timeout: 60_000,
        },
      );
      expect(result.error).toBeUndefined();
      expect(result.status).toBe(1);
      expect(`${result.stdout}${result.stderr}`).toContain(
        "changed the API signature",
      );
      expect(readFileSync(report, "utf8")).toBe(accepted);
    } finally {
      writeFileSync(inherited, before);
    }
  },
  60_000,
);

it.skipIf(!enabled)(
  "rejects a deliberate public signature change",
  () => {
    if (!existsSync(declaration)) throw new Error("Run bun run build first");
    const before = readFileSync(declaration, "utf8");
    const report = new URL("../../docs/rust/api/core.api.md", import.meta.url);
    const accepted = readFileSync(report, "utf8");
    try {
      writeFileSync(
        declaration,
        `${before}\nexport declare function incompatibleApiProbe(): never;\n`,
      );
      const result = spawnSync(
        "node",
        ["scripts/check-public-api.mjs", "--entry=core"],
        {
          cwd: root,
          encoding: "utf8",
          timeout: 60_000,
        },
      );
      expect(result.error).toBeUndefined();
      expect(result.status).toBe(1);
      expect(`${result.stdout}${result.stderr}`).toContain(
        "changed the API signature",
      );
      expect(readFileSync(report, "utf8")).toBe(accepted);
    } finally {
      writeFileSync(declaration, before);
    }
  },
  60_000,
);
