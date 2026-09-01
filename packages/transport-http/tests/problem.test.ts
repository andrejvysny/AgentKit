/**
 * The status table, for the two codes this package added to it.
 *
 * The table's exhaustiveness is a compile-time property (`satisfies
 * Record<HostErrorCode, number>`); what a test can still say is that a given
 * code lands on the status a client is expected to branch on.
 */
import { describe, expect, it } from "bun:test";
import {
  InvalidForkPointError,
  RecordNotFoundError,
  UsageDeniedError,
} from "@agentkit/host";
import { forbidden, problemForError } from "../src/problem.js";

describe("problemForError", () => {
  it("maps a usage refusal to 429, not to a 4xx that says stop asking", async () => {
    const res = problemForError(
      new UsageDeniedError("Monthly budget exhausted."),
      "/v1/runs/r1",
    );
    expect(res.status).toBe(429);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
    const body = (await res.json()) as Record<string, unknown>;
    expect(body["code"]).toBe("usage_denied");
    expect(body["title"]).toBe("Too Many Requests");
    expect(body["detail"]).toBe("Monthly budget exhausted.");
    expect(body["instance"]).toBe("/v1/runs/r1");
  });

  it("still maps the codes it always did", async () => {
    expect(problemForError(new RecordNotFoundError("x"), "/i").status).toBe(
      404,
    );
    expect(problemForError(new InvalidForkPointError("x"), "/i").status).toBe(
      400,
    );
  });
});

describe("forbidden", () => {
  it("is a transport-level 403, like not_implemented is a transport-level 501", async () => {
    const res = forbidden("Not a member of this workspace.", "/v1/chats/c1");
    expect(res.status).toBe(403);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body).toEqual({
      type: "https://agentkit.dev/problems/forbidden",
      title: "Forbidden",
      status: 403,
      detail: "Not a member of this workspace.",
      instance: "/v1/chats/c1",
      code: "forbidden",
    });
  });
});
