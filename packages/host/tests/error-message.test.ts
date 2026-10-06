import { describe, expect, it } from "bun:test";
import { publicErrorMessage } from "../src/turn/error-message.js";

describe("publicErrorMessage", () => {
  it("preserves domain errors", () => {
    expect(
      publicErrorMessage(
        new Error("execution_budget_exhausted: providerRequests"),
      ),
    ).toBe("execution_budget_exhausted: providerRequests");
  });

  it("redacts Bearer and credential fields before persistence", () => {
    const message = publicErrorMessage(
      new Error(
        'Authorization: Bearer bearer-secret {"client_secret":"client-private","access_token":"access-private"} password=password-private apiKey=key-private Authorization=Basic basic-private',
      ),
    );
    for (const secret of [
      "bearer-secret",
      "client-private",
      "access-private",
      "password-private",
      "key-private",
      "basic-private",
    ])
      expect(message).not.toContain(secret);
    expect(message).toContain("[REDACTED]");
  });

  it("bounds multibyte errors after redaction", () => {
    const message = publicErrorMessage(
      new Error(`Bearer private-token ${"🙂".repeat(3000)}`),
    );
    expect(message).not.toContain("private-token");
    expect(new TextEncoder().encode(message).byteLength).toBeLessThanOrEqual(
      4096,
    );
    expect(message).not.toContain("�");
  });
});
