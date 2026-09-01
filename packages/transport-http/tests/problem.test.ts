/**
 * The status table, for the two codes this package added to it.
 *
 * The table's exhaustiveness is a compile-time property (`satisfies
 * Record<HostErrorCode, number>`); what a test can still say is that a given
 * code lands on the status a client is expected to branch on.
 */
import { describe, expect, it } from "bun:test";
import {
  ChatBusyError,
  InvalidForkPointError,
  InvalidImportError,
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

  it("separates a chat that is busy now (409) from an import that never parses (400)", async () => {
    // A live run is a state that resolves itself; the same delete succeeds
    // later, which is what 409 tells a client and 400 would not.
    const busy = problemForError(
      new ChatBusyError("Chat c1 has 1 task(s) still running.", {
        chatId: "c1",
      }),
      "/v1/chats/c1",
    );
    expect(busy.status).toBe(409);
    expect(((await busy.json()) as Record<string, unknown>)["code"]).toBe(
      "chat_busy",
    );
    // A malformed payload will not become well-formed by waiting.
    const bad = problemForError(
      new InvalidImportError("Two active children.", {
        reason: "broken_active_chain",
      }),
      "/v1/chats/import",
    );
    expect(bad.status).toBe(400);
    expect(((await bad.json()) as Record<string, unknown>)["code"]).toBe(
      "invalid_import",
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
