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
  ExecutorNotFoundError,
  InvalidForkPointError,
  InvalidImportError,
  type Logger,
  RecordNotFoundError,
  UsageDeniedError,
} from "@agentkit/host";
import { forbidden, problemForError, unprocessable } from "../src/problem.js";

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

describe("problemForError on a server fault", () => {
  it("publishes a generic detail for a 5xx and logs the real message", async () => {
    const logged: { message: string; fields?: Record<string, unknown> }[] = [];
    const logger: Logger = {
      debug: () => {},
      info: () => {},
      warn: () => {},
      error: (message, fields) => logged.push({ message, ...{ fields } }),
    };
    // `executor_not_found` is the everyday 5xx here, and its message names the
    // task kinds this process registered — a map of the deployment's wiring,
    // handed to whoever asked for a run it could not execute.
    const res = problemForError(
      new ExecutorNotFoundError(
        'No executor registered for kind "chat.turn"; registered: index.embed, audit.replay.',
      ),
      "/v1/runs/r1",
      logger,
    );
    expect(res.status).toBe(500);
    const body = (await res.json()) as Record<string, unknown>;
    // The code still travels — a client branches on it — but the message does
    // not.
    expect(body["code"]).toBe("executor_not_found");
    expect(body["detail"]).toBe("The server failed to handle the request.");
    expect(JSON.stringify(body)).not.toContain("index.embed");
    // Nothing is lost to the operator.
    expect(logged[0]?.fields?.["message"]).toContain("index.embed");
  });

  it("keeps the host's own message on a 4xx, which is what makes it actionable", async () => {
    const res = problemForError(
      new RecordNotFoundError("Chat not found: c-missing"),
      "/v1/chats/c-missing",
    );
    expect(((await res.json()) as Record<string, unknown>)["detail"]).toBe(
      "Chat not found: c-missing",
    );
  });
});

describe("unprocessable", () => {
  it("is a transport-level 422 with the status's own title", async () => {
    const res = unprocessable(
      "idempotency_key_mismatch",
      "Reused key, different body.",
      "/v1/chats/c1/messages",
    );
    expect(res.status).toBe(422);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body["code"]).toBe("idempotency_key_mismatch");
    expect(body["title"]).toBe("Unprocessable Content");
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
