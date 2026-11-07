import { testServiceOptions } from "./helpers/test-profiles.js";
/**
 * W1a — a tool NAME is a unique registration identity, atomically across
 * BOTH registries (Rust descriptors + JS callbacks).
 *
 * The witness this pins (reproduced before the fix at head 24b3d17):
 * registering `probe` v1 with `effect:"read"` and then `probe` v2 with
 * `effect:"write"` used to leave the Rust contributor gating/advertising
 * the READ descriptor while the JS router dispatched the WRITE callback —
 * with `writesEnabled:false` the write counter ran behind a read identity.
 * Now the second registration REJECTS (stable `invalid_input`) and neither
 * registry mutates, so name, advertised descriptor, write gate, schema
 * validator, and dispatched callback all denote the ONE accepted
 * registration — in either registration order.
 *
 * Exercised at the public MCP boundary: official SDK client against the
 * Rust streamable-HTTP server, with harmless in-memory effect counters.
 */
import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import {
  NativeAgentError,
  type NativeSessionService,
  NativeStore,
} from "../src/index.js";
import type { ToolDefinitionJson } from "../src/index.js";
import { available } from "./helpers/binding.js";

const nativeTest = available ? test : test.skip;

const TOKEN = "a".repeat(32);

/** The two definitions differ in EVERY lane that keys by name: effect
 * (gate), description (advertised label), and a disjoint required property
 * (schema) so a call valid for one is never valid for the other. */
const PROBE_READ_V1: ToolDefinitionJson = {
  name: "probe",
  version: "1.0.0",
  effect: "read",
  capability: "test.probe",
  description: "read probe",
  inputSchema: {
    type: "object",
    properties: { q: { type: "string" } },
    required: ["q"],
    additionalProperties: false,
  },
};

const PROBE_WRITE_V2: ToolDefinitionJson = {
  name: "probe",
  version: "2.0.0",
  effect: "write",
  capability: "test.probe",
  description: "write probe",
  inputSchema: {
    type: "object",
    properties: { target: { type: "string" } },
    required: ["target"],
    additionalProperties: false,
  },
};

async function withService(): Promise<{
  store: NativeStore;
  service: NativeSessionService;
}> {
  const store = await NativeStore.open(
    join(mkdtempSync(join(tmpdir(), "agentkit-identity-")), "store.sqlite"),
  );
  const service = await store.createService(
    testServiceOptions({
      jsToolAuthority: { configuredMode: "full_auto" },
    }),
  );
  return { store, service };
}

function newMcpClient(
  url: string,
  token: string,
): [Client, StreamableHTTPClientTransport] {
  const client = new Client(
    { name: "native-identity-test", version: "1.0.0" },
    { capabilities: {} },
  );
  const transport = new StreamableHTTPClientTransport(new URL(url), {
    requestInit: { headers: { authorization: `Bearer ${token}` } },
  });
  return [client, transport];
}

function isErrorResponse(result: unknown): boolean {
  return (
    typeof result === "object" &&
    result !== null &&
    "isError" in result &&
    result.isError === true
  );
}

async function registerRejection(
  service: NativeSessionService,
  definition: ToolDefinitionJson,
): Promise<unknown> {
  try {
    await service.registerTool(definition, async () => ({
      data: "must-never-run",
    }));
    return null; // no rejection — the assertions below fail loudly
  } catch (error) {
    return error;
  }
}

nativeTest(
  "name identity: read-first, the write re-registration is rejected and the READ callback is the only one reachable",
  async () => {
    const { store, service } = await withService();
    let reads = 0;
    const writes = 0;
    try {
      await service.registerTool(PROBE_READ_V1, async () => {
        reads += 1;
        return { data: "read-ran" };
      });

      // (c) the second registerTool REJECTS with a stable error...
      const rejection = await registerRejection(service, PROBE_WRITE_V2);
      expect(rejection).toBeInstanceOf(NativeAgentError);
      if (rejection instanceof NativeAgentError) {
        expect(rejection.code).toBe("invalid_input");
        expect(rejection.message).toContain("tool probe is already registered");
      }

      const info = await service.startMcpServer({ bearerTokens: [TOKEN] });
      const [client, transport] = newMcpClient(info.url, TOKEN);
      await client.connect(transport);
      try {
        // ...and the advertised descriptor is the ACCEPTED (read) one:
        // one entry, read description, read schema.
        const listed = await client.listTools();
        const probes = listed.tools.filter((t) => t.name === "probe");
        expect(probes.length).toBe(1);
        expect(probes[0]?.description).toBe("read probe");
        expect(JSON.stringify(probes[0]?.inputSchema)).toBe(
          JSON.stringify(PROBE_READ_V1.inputSchema),
        );

        // (a)+(b) the call never executes the write counter and runs
        // EXACTLY the first-registered/advertised callback.
        const called = await client.callTool({
          name: "probe",
          arguments: { q: "x" },
        });
        expect(isErrorResponse(called)).toBe(false);
        expect(reads).toBe(1);
        expect(writes).toBe(0);

        // (d) the schemas stay distinct: a call schema-valid for the
        // REJECTED write version is REJECTED outright by the advertised
        // (accepted) schema — asserted as rejection — and runs neither
        // callback.
        const forged = await client.callTool({
          name: "probe",
          arguments: { target: "y" },
        });
        expect(isErrorResponse(forged)).toBe(true);
        expect(reads).toBe(1);
        expect(writes).toBe(0);

        // The rejected registration mutated NOTHING: the same accepted
        // callback still owns the name (a router overwrite would have
        // flipped this to the write counter).
        const again = await client.callTool({
          name: "probe",
          arguments: { q: "z" },
        });
        expect(isErrorResponse(again)).toBe(false);
        expect(reads).toBe(2);
        expect(writes).toBe(0);
      } finally {
        await transport.close().catch(() => undefined);
      }
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
    }
  },
  { timeout: 30_000 },
);

nativeTest(
  "name identity: write-first, the read re-registration is rejected and the WRITE registration is gated exactly as advertised",
  async () => {
    const { store, service } = await withService();
    const reads = 0;
    let writes = 0;
    try {
      await service.registerTool(PROBE_WRITE_V2, async () => {
        writes += 1;
        return { data: "write-ran" };
      });

      // (c) same stable rejection in the other order: the accepted WRITE
      // registration keeps the name.
      const rejection = await registerRejection(service, PROBE_READ_V1);
      expect(rejection).toBeInstanceOf(NativeAgentError);
      if (rejection instanceof NativeAgentError) {
        expect(rejection.code).toBe("invalid_input");
        expect(rejection.message).toContain("tool probe is already registered");
      }

      const info = await service.startMcpServer({ bearerTokens: [TOKEN] });
      const [client, transport] = newMcpClient(info.url, TOKEN);
      await client.connect(transport);
      try {
        // The single accepted registration is the WRITE one, so while
        // writesEnabled is false the name is invisible AND uncallable —
        // never advertised as a read, never executed as a callback behind
        // a read identity.
        const listed = await client.listTools();
        expect(listed.tools.filter((t) => t.name === "probe").length).toBe(0);

        // (a) the write counter never runs while the gate is closed...
        await expect(
          client.callTool({ name: "probe", arguments: { target: "y" } }),
        ).rejects.toThrow(/Unknown tool/);
        expect(writes).toBe(0);
        // ...and the rejected read version never became reachable either.
        await expect(
          client.callTool({ name: "probe", arguments: { q: "x" } }),
        ).rejects.toThrow(/Unknown tool/);
        expect(reads).toBe(0);
      } finally {
        await transport.close().catch(() => undefined);
      }
    } finally {
      await service.close().catch(() => undefined);
      await store.close();
    }
  },
  { timeout: 30_000 },
);
