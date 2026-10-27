import { describe, expect, test } from "bun:test";
import { AiToolRegistry } from "@agentkit/core";
import {
  RESERVED_TOOL_NAMESPACES,
  TOOL_NAMESPACE_PATTERN,
} from "@agentkit/host";
import { CODING_TOOL_NAMES, createCodingToolPack } from "../src/index.js";

const EXPECTED: Record<string, [string, string]> = {
  read_file: ["read", "fs.read"],
  list_dir: ["read", "fs.read"],
  find_files: ["read", "fs.read"],
  code_search: ["read", "fs.read"],
  repo_map: ["read", "fs.read"],
  git_inspect: ["read", "git.read"],
  edit_file: ["write", "fs.write"],
  write_file: ["write", "fs.write"],
  run_command: ["write", "process.exec"],
  run_tests: ["write", "process.exec"],
  submit_plan: ["read", "plan"],
};

describe("createCodingToolPack", () => {
  const pack = createCodingToolPack({ workspace: { root: "/tmp" } });
  const limits = { profile: "small", maxBytes: 16384 } as const;

  test("registers 11 tools under the valid, unreserved namespace coding", async () => {
    expect(pack.namespace).toBe("coding");
    expect(TOOL_NAMESPACE_PATTERN.test(pack.namespace)).toBe(true);
    expect(RESERVED_TOOL_NAMESPACES).not.toContain(pack.namespace);
    const tools = await pack.contribute({ bindings: [], limits });
    expect(tools.map((t) => t.definition.name)).toEqual([...CODING_TOOL_NAMES]);
    expect(tools).toHaveLength(11);
  });

  test("effect, capability, timeout, description and strict schemas", async () => {
    for (const tool of await pack.contribute({ bindings: [], limits })) {
      const d = tool.definition;
      expect([d.effect, d.capability]).toEqual(EXPECTED[d.name] as string[]);
      expect(d.timeoutMs).toBeGreaterThan(0);
      expect(d.description.length).toBeGreaterThan(20);
      expect(d.inputSchema.additionalProperties).toBe(false);
    }
  });

  test("every schema compiles in AiToolRegistry and rejects extra properties", async () => {
    const registry = new AiToolRegistry();
    for (const tool of await pack.contribute({ bindings: [], limits }))
      registry.register(tool);
    expect(registry.size()).toBe(11);
    expect(registry.validateInput("submit_plan", { plan: "do it" })).toEqual(
      [],
    );
    expect(
      registry.validateInput("submit_plan", { plan: "do it", extra: 1 }),
    ).not.toEqual([]);
    expect(registry.validateInput("submit_plan", {})).not.toEqual([]);
    expect(registry.validateInput("read_file", { path: "a.ts" })).toEqual([]);
    expect(
      registry.validateInput("read_file", { path: "a.ts", x: 1 }),
    ).not.toEqual([]);
    expect(registry.validateInput("read_file", {})).not.toEqual([]);
    expect(
      registry.validateInput("git_inspect", { command: "push" }),
    ).not.toEqual([]);
    expect(
      registry.validateInput("git_inspect", {
        command: "log",
        args: { maxCount: 51 },
      }),
    ).not.toEqual([]);
    expect(
      registry.validateInput("run_command", { command: "ls", timeoutSec: 601 }),
    ).not.toEqual([]);
    expect(
      registry.validateInput("edit_file", { path: "a", edits: [] }),
    ).not.toEqual([]);
    expect(
      registry.validateInput("code_search", { pattern: "x", context: 21 }),
    ).not.toEqual([]);
  });

  test("descriptions steer the model to native tools", async () => {
    const tools = await pack.contribute({ bindings: [], limits });
    const text = (n: string) =>
      tools.find((t) => t.definition.name === n)?.definition.description ?? "";
    expect(text("run_command")).toContain(
      "Do NOT use it to read or search files",
    );
    expect(text("code_search")).toContain("instead of grep");
    expect(text("read_file")).toContain("Prefer the native tools");
  });

  test("rejects a relative root and bad limits", () => {
    expect(() =>
      createCodingToolPack({ workspace: { root: "rel/path" } }),
    ).toThrow(TypeError);
    expect(() =>
      createCodingToolPack({
        workspace: { root: "/tmp" },
        limits: { readMaxLines: 0 },
      }),
    ).toThrow(TypeError);
  });
});
