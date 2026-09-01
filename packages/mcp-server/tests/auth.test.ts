import { describe, expect, it } from "bun:test";
import { timingSafeEqualString, verifyBearer } from "../src/auth.js";
import { createMcpServerHandler } from "../src/index.js";
import type { McpToolSource } from "../src/index.js";

const NULL_SOURCE: McpToolSource = {
  catalog: { listTools: async () => [] },
  execute: async () => {
    throw new Error("unreachable");
  },
};

describe("timingSafeEqualString", () => {
  it("is true only for identical strings", async () => {
    expect(await timingSafeEqualString("abc", "abc")).toBe(true);
    expect(await timingSafeEqualString("abc", "abd")).toBe(false);
  });

  it("answers for unequal-length inputs without a length short-circuit", async () => {
    // This is the whole reason both sides are hashed: an early
    // `a.length !== b.length` return answers "your guess is the wrong length",
    // which is exactly the bit an attacker wants. Digesting makes every
    // comparison 32 bytes wide, so these all take the same path.
    expect(await timingSafeEqualString("short", "a-much-longer-token")).toBe(
      false,
    );
    expect(await timingSafeEqualString("", "x")).toBe(false);
    expect(await timingSafeEqualString("", "")).toBe(true);
  });

  it("compares unicode by value, not by byte length", async () => {
    expect(await timingSafeEqualString("héllo", "héllo")).toBe(true);
    expect(await timingSafeEqualString("héllo", "hello")).toBe(false);
  });
});

describe("verifyBearer", () => {
  it("accepts a correct token, scheme case-insensitively", async () => {
    expect(await verifyBearer("Bearer tok", "tok")).toBe(true);
    expect(await verifyBearer("bearer tok", "tok")).toBe(true);
    expect(await verifyBearer("BEARER  tok", "tok")).toBe(true);
  });

  it("refuses a missing header, another scheme, and an empty token", async () => {
    expect(await verifyBearer(null, "tok")).toBe(false);
    expect(await verifyBearer("Basic dG9r", "tok")).toBe(false);
    expect(await verifyBearer("Bearer", "tok")).toBe(false);
    expect(await verifyBearer("Bearer ", "tok")).toBe(false);
  });

  it("refuses a wrong token of the same length", async () => {
    expect(await verifyBearer("Bearer aaaa", "aaab")).toBe(false);
  });
});

describe("createMcpServerHandler auth wiring", () => {
  it("refuses an empty bearerToken at wiring time", () => {
    expect(() =>
      createMcpServerHandler({ tools: NULL_SOURCE, auth: { bearerToken: "" } }),
    ).toThrow(/bearerToken is empty/);
    expect(() =>
      createMcpServerHandler({
        tools: NULL_SOURCE,
        auth: { bearerToken: "   " },
      }),
    ).toThrow(/bearerToken is empty/);
  });

  it("supports a custom verify(), including an async one", async () => {
    const seen: (string | null)[] = [];
    const handler = createMcpServerHandler({
      tools: NULL_SOURCE,
      auth: {
        async verify(header) {
          seen.push(header);
          return header === "Custom yes";
        },
      },
    });
    const refused = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: { host: "localhost" },
      }),
    );
    expect(refused.status).toBe(401);
    expect(seen).toEqual([null]);
    await handler.dispose();
  });

  it("fails closed when a custom verify() throws", async () => {
    const handler = createMcpServerHandler({
      tools: NULL_SOURCE,
      auth: {
        verify() {
          throw new Error("auth backend is down");
        },
      },
    });
    const response = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: { host: "localhost", authorization: "Bearer whatever" },
      }),
    );
    // 401, not a rejected promise the surrounding server turns into a 500.
    expect(response.status).toBe(401);
    await handler.dispose();
  });
});
