import { describe, expect, it } from "bun:test";
import {
  ResponsesClient,
  ResponsesProviderError,
  type ResponsesErrorDiagnostic,
} from "../src/index.js";
import { call, collect, harness, terminal } from "./responses-helpers.js";

function failed(events: Awaited<ReturnType<typeof collect>>) {
  const event = events.find((event) => event.type === "run.failed");
  return event?.type === "run.failed" ? event.data.errorCode : undefined;
}

const cases = [
  ["subscription_sharing_usage_limit_exceeded", "usage_limit", 429, false],
  ["subscription_sharing_usage_unavailable", "service_unavailable", 503, true],
  ["subscription_sharing_user_unavailable", "service_unavailable", 503, true],
  ["subscription_sharing_user_not_eligible", "permission_denied", 403, false],
  [
    "subscription_sharing_unsupported_capability",
    "unsupported_parameter",
    400,
    false,
  ],
  ["subscription_sharing_route_not_supported", "unsupported_route", 403, false],
  ["subscription_sharing_invalid_user", "authentication_required", 401, false],
  ["chatpass_v2_scope_not_authorized", "consent_required", 403, false],
] as const;

describe("Responses safe recovery diagnostics", () => {
  it.each(cases)(
    "HTTP %s keeps exact metadata",
    async (providerCode, code, status, retryable) => {
      const { transport } = harness();
      let calls = 0;
      transport.request = async () => {
        calls++;
        return Response.json(
          {
            error: {
              code: providerCode,
              param: "tools[0].type",
              message: "Bearer secret-token",
            },
          },
          { status, headers: { "x-request-id": "req_fixture" } },
        );
      };
      const diagnostics: ResponsesErrorDiagnostic[] = [];
      const events = await collect(
        new ResponsesClient({
          id: "provider",
          transport,
          onError: (value) => diagnostics.push(value),
        }),
      );
      expect(failed(events)).toBe(code);
      expect(diagnostics).toEqual([
        {
          httpStatus: status,
          providerCode,
          param: "tools[0].type",
          requestId: "req_fixture",
          responseShape: "error",
          retryable,
        },
      ]);
      expect(calls).toBe(1);
      expect(JSON.stringify({ events, diagnostics })).not.toContain(
        "secret-token",
      );
      expect(
        events.some(
          (event) =>
            event.type === "run.completed" ||
            event.type === "run.tool.requested",
        ),
      ).toBe(false);
    },
  );

  it.each(cases)(
    "late terminal SSE %s never succeeds",
    async (providerCode, code, _status, retryable) => {
      for (const error of [
        {
          type: "error",
          code: providerCode,
          param: "model",
          message: "secret-token",
        },
        {
          type: "response.failed",
          response: {
            error: {
              code: providerCode,
              param: "model",
              message: "secret-token",
            },
          },
        },
      ]) {
        const { transport } = harness([terminal([call("one")]), error]);
        const diagnostics: ResponsesErrorDiagnostic[] = [];
        const events = await collect(
          new ResponsesClient({
            id: "provider",
            transport,
            onError: (value) => diagnostics.push(value),
          }),
        );
        expect(failed(events)).toBe(code);
        expect(diagnostics).toEqual([
          {
            httpStatus: 200,
            requestId: undefined,
            providerCode,
            param: "model",
            responseShape: "sse",
            retryable,
          },
        ]);
        expect(
          events.some(
            (event) =>
              event.type === "run.completed" ||
              event.type === "run.message.completed" ||
              event.type === "run.tool.requested",
          ),
        ).toBe(false);
        expect(JSON.stringify({ events, diagnostics })).not.toContain(
          "secret-token",
        );
      }
    },
  );

  it.each([401, 403, 503])(
    "direct HTTP %i detail body is never diagnostic text",
    async (status) => {
      const { transport } = harness();
      transport.request = async () =>
        Response.json(
          { detail: "Bearer secret-token" },
          { status, headers: { "x-request-id": "req_direct" } },
        );
      const diagnostics: ResponsesErrorDiagnostic[] = [];
      const events = await collect(
        new ResponsesClient({
          id: "provider",
          transport,
          onError: (value) => diagnostics.push(value),
        }),
      );
      expect(failed(events)).toBe(
        status === 401
          ? "authentication_required"
          : status === 403
            ? "consent_required"
            : "service_unavailable",
      );
      expect(diagnostics[0]).toMatchObject({
        httpStatus: status,
        requestId: "req_direct",
        responseShape: "detail",
        retryable: status === 503,
      });
      expect(JSON.stringify({ events, diagnostics })).not.toContain(
        "secret-token",
      );
    },
  );

  it("bounds malformed HTTP diagnostics and ignores observer failures", async () => {
    const { transport } = harness();
    transport.request = async () =>
      Response.json(
        {
          error: {
            code: "x".repeat(257),
            param: "Bearer secret-token",
            message: "secret-token",
          },
        },
        { status: 503, headers: { "x-request-id": "x".repeat(257) } },
      );
    let diagnostic: ResponsesErrorDiagnostic | undefined;
    const events = await collect(
      new ResponsesClient({
        id: "provider",
        transport,
        onError(value) {
          diagnostic = value;
          throw new Error("observer-secret");
        },
      }),
    );
    expect(failed(events)).toBe("service_unavailable");
    expect(diagnostic).toEqual({
      httpStatus: 503,
      requestId: undefined,
      providerCode: undefined,
      param: undefined,
      responseShape: "error",
      retryable: true,
    });
    expect(JSON.stringify(events)).not.toContain("secret");
  });

  it("unreadable HTTP body retains status without leaking text", async () => {
    const { transport } = harness();
    transport.request = async () =>
      new Response("secret-token", { status: 503 });
    const diagnostics: ResponsesErrorDiagnostic[] = [];
    const events = await collect(
      new ResponsesClient({
        id: "provider",
        transport,
        onError: (value) => diagnostics.push(value),
      }),
    );
    expect(failed(events)).toBe("service_unavailable");
    expect(diagnostics[0]?.responseShape).toBe("unreadable");
  });

  it("model catalog rejection carries structured diagnostics only", async () => {
    const { transport } = harness();
    transport.request = async () =>
      Response.json(
        {
          error: {
            code: "subscription_sharing_usage_unavailable",
            param: "model",
            message: "secret-token",
          },
        },
        { status: 503, headers: { "x-request-id": "req_models" } },
      );
    try {
      await new ResponsesClient({ id: "provider", transport }).listModels();
      throw new Error("Expected model rejection");
    } catch (error) {
      expect(error).toBeInstanceOf(ResponsesProviderError);
      if (!(error instanceof ResponsesProviderError)) throw error;
      expect(error.code).toBe("service_unavailable");
      expect(error.diagnostic).toMatchObject({
        httpStatus: 503,
        providerCode: "subscription_sharing_usage_unavailable",
        param: "model",
        requestId: "req_models",
        retryable: true,
      });
      expect(error.message).not.toContain("secret-token");
    }
  });
});
