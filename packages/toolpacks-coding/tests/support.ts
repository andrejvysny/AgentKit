import {
  mkdirSync,
  mkdtempSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import type { AiToolResult } from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import {
  type CodingToolPackOptions,
  createCodingToolPack,
} from "../src/index.js";

const created: string[] = [];

/** A canonical temp directory, removed by `cleanup()`. */
export function tempDir(prefix = "coding-test-"): string {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), prefix)));
  created.push(dir);
  return dir;
}

export function cleanup(): void {
  for (const dir of created.splice(0))
    rmSync(dir, { recursive: true, force: true });
}

export function put(root: string, rel: string, content: string): string {
  const path = join(root, rel);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, content);
  return path;
}

const LIMITS = { profile: "small", maxBytes: 1 << 20 } as const;

export interface Harness {
  root: string;
  run(
    name: string,
    input: unknown,
    signal?: AbortSignal,
  ): Promise<AiToolResult<unknown>>;
  text(result: AiToolResult<unknown>): string;
  dispose(): Promise<void>;
}

export async function harness(
  options: Partial<CodingToolPackOptions> & { root?: string } = {},
): Promise<Harness> {
  const root = options.root ?? tempDir();
  const { root: _ignored, ...rest } = options;
  const pack = createCodingToolPack({
    workspace: { root },
    sandbox: "off",
    ...rest,
  });
  const tools = new Map<string, AiTool>();
  for (const tool of await pack.contribute({ bindings: [], limits: LIMITS }))
    tools.set(tool.definition.name, tool);
  return {
    root,
    async run(name, input, signal) {
      const tool = tools.get(name);
      if (!tool) throw new Error(`no tool ${name}`);
      return tool.execute(
        { runId: "t", bindings: [], limits: LIMITS, signal },
        input,
      );
    },
    text: (result) => String(result.modelData),
    dispose: async () => pack.dispose?.(),
  };
}
