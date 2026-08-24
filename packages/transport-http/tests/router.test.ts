/**
 * The matcher, and the two answers it gives when nothing matches.
 *
 * The 405 case carries the weight: a client that used the wrong verb on a real
 * path needs to be told what the path DOES accept, and `Allow` is where a
 * generic HTTP client looks for that.
 */
import { describe, expect, it } from "bun:test";
import { REST_ROUTES } from "@agentkit/contracts";
import { matchRoute } from "../src/router.js";
import { createHandlerFixture, request } from "./support/fixture.js";

describe("matchRoute", () => {
  it("(a) resolves every route in the contract table", () => {
    for (const [operation, def] of Object.entries(REST_ROUTES)) {
      // Substitute a value for each `:param` so the concrete path is real.
      const path = def.path
        .split("/")
        .map((segment) =>
          segment.startsWith(":") ? `${segment.slice(1)}-value` : segment,
        )
        .join("/");
      const match = matchRoute(def.method, path);
      expect(match.kind).toBe("matched");
      if (match.kind !== "matched") continue;
      expect(match.operation).toBe(operation as never);
    }
  });

  it("(b) extracts path parameters, percent-decoded", () => {
    const match = matchRoute("GET", "/v1/chats/chat%2F1/messages");
    expect(match.kind).toBe("matched");
    if (match.kind !== "matched") return;
    expect(match.operation).toBe("listMessages");
    expect(match.params["chatId"]).toBe("chat/1");
  });

  it("(c) reports the methods a known path does accept", () => {
    const match = matchRoute("DELETE", "/v1/chats");
    expect(match.kind).toBe("method_not_allowed");
    if (match.kind !== "method_not_allowed") return;
    expect([...match.allow].sort()).toEqual(["GET", "POST"]);
  });

  it("(d) does not confuse a longer path with a prefix of it", () => {
    expect(matchRoute("GET", "/v1/chats/c1/messages/extra").kind).toBe(
      "not_found",
    );
    expect(matchRoute("GET", "/v1/nope").kind).toBe("not_found");
  });
});

describe("handler routing", () => {
  it("(e) answers an unknown path with a 404 problem document", async () => {
    const { handler } = await createHandlerFixture();
    const res = await handler(request("GET", "/v1/nope"));
    expect(res.status).toBe(404);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
    const body = (await res.json()) as Record<string, unknown>;
    expect(body["code"]).toBe("not_found");
    expect(body["status"]).toBe(404);
    expect(body["type"]).toBe("https://agentkit.dev/problems/not_found");
    expect(body["instance"]).toBe("/v1/nope");
  });

  it("(f) answers a wrong method with 405 and an Allow header", async () => {
    const { handler } = await createHandlerFixture();
    const res = await handler(request("DELETE", "/v1/chats"));
    expect(res.status).toBe(405);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
    const allow = (res.headers.get("allow") ?? "").split(", ").sort();
    expect(allow).toEqual(["GET", "POST"]);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body["code"]).toBe("method_not_allowed");
  });
});
