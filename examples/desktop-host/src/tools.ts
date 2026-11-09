/**
 * The tools this example ships: two read-only tools that exist purely to show
 * the round trip — a JSON-Schema `inputSchema`, a `run` function, and the
 * slim tool-result envelope every tool returns regardless of what it does.
 * Copy this file's shape for a real tool; the Rust engine validates
 * `inputSchema` against the descriptor before `run` ever sees the arguments
 * (`JsToolGuard` in `rust/crates/napi`, mirroring `AiToolRegistry`'s Ajv
 * validation on the TypeScript side).
 *
 * NATIVE COMPOSITION: tools are registered once at boot through
 * `NativeSessionService.registerTool(definition, run)` — the JS tool lane.
 * Every call is bridged into this process from Rust, judged by the Rust-side
 * authority chain, and its result crosses back as the host tool-result JSON.
 * There is no per-run contributor here any more: registration IS the
 * contribution (see `wiring.ts` step 4 for how the catalogue the REST route
 * advertises is built from this exact list).
 */
import type {
  ToolDefinitionJson,
  ToolResultJson,
  ToolRunFunction,
} from "@agentkit/native";
import type { Clock } from "./system-types.js";

/** One tool and its execution, as one unit: both halves go to
 * `registerTool`, and the catalogue entry the façade serves is the
 * `definition` — one source of truth, no drift possible. */
export interface ExampleTool {
  definition: ToolDefinitionJson;
  run: ToolRunFunction;
}

/**
 * Named `example_echo`, not `example.echo`: the descriptor name is validated
 * against `TOOL_NAME_PATTERN` in `@agentkit/contracts` —
 * `^[a-zA-Z0-9_-]+$` ("no dots or spaces") — by `registerTool` before the
 * Rust registry ever sees it. Underscore is this framework's flat-name
 * convention. The dotted form still lives in `capability`, which has no such
 * restriction.
 */
const echoTool: ExampleTool = {
  definition: {
    name: "example_echo",
    version: "1.0.0",
    effect: "read",
    capability: "example.echo",
    description: "Echo the given text back unchanged.",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
      additionalProperties: false,
    },
  },
  run: (input) => {
    const text =
      input !== null && typeof input === "object" && "text" in input
        ? String(input.text)
        : "";
    return {
      data: { echoed: text },
      summary: `echoed "${text}"`,
      sources: [],
      warnings: [],
      truncated: false,
    } satisfies ToolResultJson;
  },
};

/** Takes `clock` rather than reading `Date.now()` — see `packages/host/src/ports/system.ts`. */
function createNowTool(clock: Clock): ExampleTool {
  return {
    definition: {
      name: "example_now",
      version: "1.0.0",
      effect: "read",
      capability: "example.now",
      description: "Return the current time as an ISO-8601 string.",
      inputSchema: {
        type: "object",
        properties: {},
        additionalProperties: false,
      },
    },
    run: () => {
      const now = clock.nowIso();
      return {
        data: { now },
        summary: now,
        sources: [],
        warnings: [],
        truncated: false,
      } satisfies ToolResultJson;
    },
  };
}

/**
 * The one set this example registers. Both tools are `effect: "read"`, so
 * the Rust authority chain admits them without approvals in any mode; a
 * `write`-effect tool would park on the durable ask unless the host widens
 * `jsToolAuthority` (see `wiring.ts` step 4 for why this example runs
 * `full_auto`).
 */
export function createExampleTools(clock: Clock): ExampleTool[] {
  return [echoTool, createNowTool(clock)];
}
