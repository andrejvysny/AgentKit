/**
 * The AGENTKIT-90 teammate lane: the fifteen routes behind
 * `deps.teammates`.
 *
 * Three things are graded here, in the order the deployment meets them.
 * FIRST the 501: the routes exist in the contract and an unwired host must
 * say `not_implemented`, not 404 — the `listTools` rule, per route. SECOND
 * the port: every route must actually consult it (a route that answers 200
 * from a constant is a stub wearing a contract), with the bounded query it
 * was handed and the validated body — including the closed-object refusals
 * that make the definition a smuggling gate rather than a suggestion.
 * THIRD authorization: the resource each route presents to the port is
 * `agent` or `agent_task`, with the id its PATH carries — a body-named id
 * never reaches the authorizer, and this is where that would show up.
 */
import { describe, expect, it } from "bun:test";
import {
  REST_ROUTES,
  type AgentDefinitionDto,
  type AgentDto,
  type AgentTaskDto,
  type RestOperation,
  type SubmitTeammateTaskResponse,
} from "@agentkit/contracts";
import { AgentKitHostError, type AuthorizationPort } from "@agentkit/host";
import type { RestHandlerDeps } from "../src/index.js";
import type { TeammateApiPort } from "../src/deps.js";
import { createHandlerFixture, request } from "./support/fixture.js";

const AGENT_ID = "ag-1";
const TASK_ID = "at-1";

const DEFINITION: AgentDefinitionDto = {
  name: "Scribe",
  purpose: "Daily digests",
  instructions: "Summarize the day.",
  resources: [],
  skills: [],
  permissionMode: "ask",
  allowedTools: [],
  budgets: {},
  secretRefs: [],
};

const AGENT: AgentDto = {
  agentId: AGENT_ID,
  scope: "workspace",
  ownerId: "owner-1",
  lifecycle: "active",
  currentRevision: 1,
  definition: DEFINITION,
  createdAt: 1_700_000_000_000,
  updatedAt: 1_700_000_000_000,
};

const TASK: AgentTaskDto = {
  taskId: TASK_ID,
  agentId: AGENT_ID,
  agentRevision: 1,
  admissionKey: "ak-1",
  kind: "digest",
  // `completed` doubles as the stream's close signal: the pump drains the
  // empty log, writes `agentkit.stream.settled` and ends, so the SSE test
  // asserts a finished stream instead of racing a live one.
  status: "completed",
  priority: 0,
  enqueuedAt: 1_700_000_000_000,
  availableAt: 1_700_000_000_000,
  attemptCount: 1,
  cancelRequested: false,
  dependsOn: [],
};

const TEAMMATE_OPERATIONS = [
  "createAgent",
  "listAgents",
  "getAgent",
  "updateAgent",
  "setAgentModel",
  "archiveAgent",
  "restoreAgent",
  "listAgentRevisions",
  "submitTeammateTask",
  "listAgentTasks",
  "getAgentTask",
  "cancelAgentTask",
  "listAgentTaskEvents",
  "streamAgentTask",
  "getAgentTaskRunProfile",
] as const satisfies readonly RestOperation[];

/** The route path with its id holes filled the way the stub answers. */
function teammatePath(operation: RestOperation): string {
  return REST_ROUTES[operation].path
    .replace(":agentId", AGENT_ID)
    .replace(":taskId", TASK_ID);
}

interface PortCall {
  method: string;
  args: unknown[];
}

/**
 * A port that answers like a wired host and records every consultation.
 * Unknown ids answer `null` — the port's not-found convention — and a stale
 * `expectedRevision` throws the host error the dispatcher maps to 409.
 */
function stubPort(
  calls: PortCall[],
  options: { replayed?: boolean } = {},
): TeammateApiPort {
  return {
    createAgent: async (request) => {
      calls.push({ method: "createAgent", args: [request] });
      return AGENT;
    },
    listAgents: async (query) => {
      calls.push({ method: "listAgents", args: [query] });
      return [AGENT];
    },
    getAgent: async (agentId) => {
      calls.push({ method: "getAgent", args: [agentId] });
      return agentId === AGENT_ID ? AGENT : null;
    },
    updateAgent: async (agentId, request) => {
      calls.push({ method: "updateAgent", args: [agentId, request] });
      if (agentId !== AGENT_ID) return null;
      if (request.expectedRevision !== AGENT.currentRevision) {
        throw new AgentKitHostError(
          "revision_conflict",
          `expectedRevision ${request.expectedRevision} is stale; current is ${AGENT.currentRevision}.`,
        );
      }
      return AGENT;
    },
    setAgentModel: async (agentId, request) => {
      calls.push({ method: "setAgentModel", args: [agentId, request] });
      return agentId === AGENT_ID ? AGENT : null;
    },
    archiveAgent: async (agentId) => {
      calls.push({ method: "archiveAgent", args: [agentId] });
      return agentId === AGENT_ID ? { ...AGENT, lifecycle: "archived" } : null;
    },
    restoreAgent: async (agentId) => {
      calls.push({ method: "restoreAgent", args: [agentId] });
      return agentId === AGENT_ID ? AGENT : null;
    },
    listAgentRevisions: async (agentId, query) => {
      calls.push({ method: "listAgentRevisions", args: [agentId, query] });
      return agentId === AGENT_ID
        ? [
            {
              agentId,
              revision: 1,
              snapshot: DEFINITION,
              source: "create",
              createdAt: AGENT.createdAt,
            },
          ]
        : [];
    },
    submitTeammateTask: async (agentId, request) => {
      calls.push({ method: "submitTeammateTask", args: [agentId, request] });
      if (agentId !== AGENT_ID) return null;
      const answer: SubmitTeammateTaskResponse = {
        task: TASK,
        replayed: options.replayed ?? false,
      };
      return answer;
    },
    listAgentTasks: async (agentId, query) => {
      calls.push({ method: "listAgentTasks", args: [agentId, query] });
      return agentId === AGENT_ID ? [TASK] : [];
    },
    getAgentTask: async (taskId) => {
      calls.push({ method: "getAgentTask", args: [taskId] });
      return taskId === TASK_ID ? TASK : null;
    },
    cancelAgentTask: async (taskId) => {
      calls.push({ method: "cancelAgentTask", args: [taskId] });
      return taskId === TASK_ID ? { ...TASK, cancelRequested: true } : null;
    },
    listAgentTaskEvents: async (taskId, query) => {
      calls.push({ method: "listAgentTaskEvents", args: [taskId, query] });
      return { events: [] };
    },
    taskStatus: async (taskId) => {
      calls.push({ method: "taskStatus", args: [taskId] });
      return taskId === TASK_ID ? TASK.status : null;
    },
    events: async (taskId, opts) => {
      calls.push({ method: "events", args: [taskId, opts] });
      return [];
    },
    getAgentTaskRunProfile: async (taskId) => {
      calls.push({ method: "getAgentTaskRunProfile", args: [taskId] });
      return taskId === TASK_ID ? { taskId } : null;
    },
  };
}

interface SeenAuthorization {
  action: string;
  kind: string;
  id?: string;
}

function recordingAuthorize(seen: SeenAuthorization[]): AuthorizationPort {
  return {
    async authorize(input) {
      seen.push({
        action: input.action,
        kind: input.resource.kind,
        ...(input.resource.id === undefined ? {} : { id: input.resource.id }),
      });
      return { allowed: true };
    },
  };
}

function teammateDeps(
  calls: PortCall[],
  seen: SeenAuthorization[],
  options: { replayed?: boolean } = {},
): Partial<RestHandlerDeps> {
  return {
    teammates: stubPort(calls, options),
    authorize: recordingAuthorize(seen),
  };
}

/** A body the closed validators accept, per operation. */
function bodyFor(operation: RestOperation): unknown | undefined {
  switch (operation) {
    case "createAgent":
      return { definition: DEFINITION };
    case "updateAgent":
      return { expectedRevision: 1, definition: DEFINITION };
    case "setAgentModel":
      return { expectedRevision: 1, model: "gpt-4o" };
    case "submitTeammateTask":
      return {
        admissionKey: "ak-1",
        kind: "digest",
        payload: { day: "today" },
      };
    default:
      return undefined;
  }
}

async function expectProblem(
  res: Response,
  status: number,
  code: string,
): Promise<Record<string, unknown>> {
  expect(res.status).toBe(status);
  expect(res.headers.get("content-type")).toBe("application/problem+json");
  const body = (await res.json()) as Record<string, unknown>;
  expect(body["code"]).toBe(code);
  expect(body["status"]).toBe(status);
  return body;
}

describe("teammate routes without a wired port", () => {
  for (const operation of TEAMMATE_OPERATIONS) {
    it(`${operation} answers 501 not_implemented`, async () => {
      const { handler } = await createHandlerFixture();
      const route = REST_ROUTES[operation];
      const res = await handler(
        request(route.method, teammatePath(operation), {
          ...(bodyFor(operation) === undefined
            ? {}
            : { body: bodyFor(operation) }),
        }),
      );
      // The 501 must outrank the body check too: a route that cannot answer
      // at all has no business parsing what it will never read.
      await expectProblem(res, 501, "not_implemented");
    });
  }
});

describe("teammate routes through a stub port", () => {
  it("each route consults the port and answers with what it returns", async () => {
    const calls: PortCall[] = [];
    const seen: SeenAuthorization[] = [];
    const f = await createHandlerFixture(teammateDeps(calls, seen));
    for (const operation of TEAMMATE_OPERATIONS) {
      const res = await handlerCall(f, operation);
      if (operation === "streamAgentTask") {
        expect(res.status).toBe(200);
        expect(res.headers.get("content-type")).toBe("text/event-stream");
        // The pump closes on the port's terminal status — the settled frame
        // is the proof it ran against the STUB's log, not a constant.
        expect(await res.text()).toContain("agentkit.stream.settled");
      } else if (operation === "submitTeammateTask") {
        // First creation: 201.
        expect(res.status).toBe(201);
      } else if (operation === "createAgent") {
        // The identity is minted: 201.
        expect(res.status).toBe(201);
      } else if (operation === "cancelAgentTask") {
        expect(res.status).toBe(202);
      } else {
        expect(res.status, operation).toBe(200);
      }
    }
    const consulted = new Set(calls.map((call) => call.method));
    for (const operation of TEAMMATE_OPERATIONS) {
      // The stream consults the port THROUGH the pump's two reads — status
      // and pages — not under its own name.
      const required =
        operation === "streamAgentTask"
          ? ["taskStatus", "events"]
          : [operation];
      for (const method of required) {
        expect(consulted.has(method), `${operation} → ${method}`).toBe(true);
      }
    }
    // The bounded queries arrive at the port, not just the path.
    expect(calls.find((c) => c.method === "listAgents")?.args[0]).toEqual({});
  });

  it("authorizes every route against agent/agent_task with the PATH id", async () => {
    const calls: PortCall[] = [];
    const seen: SeenAuthorization[] = [];
    const f = await createHandlerFixture(teammateDeps(calls, seen));
    for (const operation of TEAMMATE_OPERATIONS) {
      await handlerCall(f, operation);
    }
    const byOperation = seen.filter(
      (entry) => entry.kind === "agent" || entry.kind === "agent_task",
    );
    // One authorization per route, in order.
    expect(byOperation).toHaveLength(TEAMMATE_OPERATIONS.length);
    for (const [index, operation] of TEAMMATE_OPERATIONS.entries()) {
      const entry = byOperation[index]!;
      const route = REST_ROUTES[operation];
      expect(entry.action, operation).toBe(
        route.method === "GET" ? "read" : "write",
      );
      if (operation === "createAgent" || operation === "listAgents") {
        // Collection routes: the kind, no id — and the stub port answered
        // without ever being given an id to leak from.
        expect(entry.kind, operation).toBe("agent");
        expect(entry.id, operation).toBeUndefined();
      } else if (operation === "submitTeammateTask") {
        // Authorized as the AGENT it admits into; the task did not exist
        // when the authorizer ruled.
        expect(entry.kind).toBe("agent");
        expect(entry.id).toBe(AGENT_ID);
      } else if (route.path.includes(":agentId")) {
        expect(entry.kind, operation).toBe("agent");
        expect(entry.id, operation).toBe(AGENT_ID);
      } else {
        expect(entry.kind, operation).toBe("agent_task");
        expect(entry.id, operation).toBe(TASK_ID);
      }
    }
  });

  it("a replayed admission answers 200, and `replayed` is the wire's proof", async () => {
    const calls: PortCall[] = [];
    const seen: SeenAuthorization[] = [];
    const f = await createHandlerFixture(
      teammateDeps(calls, seen, { replayed: true }),
    );
    const res = await f.handler(
      request("POST", `/v1/agents/${AGENT_ID}/tasks`, {
        body: bodyFor("submitTeammateTask"),
      }),
    );
    expect(res.status).toBe(200);
    const answer = (await res.json()) as SubmitTeammateTaskResponse;
    expect(answer.replayed).toBe(true);
    expect(answer.task.taskId).toBe(TASK_ID);
  });

  it("404s ids the port does not know — including the pre-stream existence check", async () => {
    const calls: PortCall[] = [];
    const seen: SeenAuthorization[] = [];
    const f = await createHandlerFixture(teammateDeps(calls, seen));
    await expectProblem(
      await f.handler(request("GET", "/v1/agents/missing")),
      404,
      "not_found",
    );
    await expectProblem(
      await f.handler(request("GET", "/v1/agent-tasks/missing")),
      404,
      "not_found",
    );
    await expectProblem(
      await f.handler(request("POST", "/v1/agent-tasks/missing/cancel")),
      404,
      "not_found",
    );
    await expectProblem(
      await f.handler(request("GET", "/v1/agent-tasks/missing/run-profile")),
      404,
      "not_found",
    );
    // The events page and the stream both check existence FIRST: an empty
    // page of nothing and an SSE stream that never ends are both worse than
    // an honest 404.
    await expectProblem(
      await f.handler(request("GET", "/v1/agent-tasks/missing/events")),
      404,
      "not_found",
    );
    await expectProblem(
      await f.handler(request("GET", "/v1/agent-tasks/missing/stream")),
      404,
      "not_found",
    );
  });

  it("409s a stale expectedRevision through the port's host error", async () => {
    const calls: PortCall[] = [];
    const seen: SeenAuthorization[] = [];
    const f = await createHandlerFixture(teammateDeps(calls, seen));
    const body = await expectProblem(
      await f.handler(
        request("PATCH", `/v1/agents/${AGENT_ID}`, {
          body: { expectedRevision: 0, definition: DEFINITION },
        }),
      ),
      409,
      "revision_conflict",
    );
    expect(body["detail"]).toBeString();
  });

  it("400s a definition that tries to smuggle a member the closed DTO lacks", async () => {
    const calls: PortCall[] = [];
    const seen: SeenAuthorization[] = [];
    const f = await createHandlerFixture(teammateDeps(calls, seen));
    const smuggled = {
      ...DEFINITION,
      apiKey: "sk-live-never-publish-me",
    };
    await expectProblem(
      await f.handler(
        request("POST", "/v1/agents", { body: { definition: smuggled } }),
      ),
      400,
      "invalid_request",
    );
    // The port was never consulted: the refusal is the boundary's, ahead of
    // anything that could store the smuggled value.
    expect(calls.filter((c) => c.method === "createAgent")).toHaveLength(0);
    await expectProblem(
      await f.handler(
        request("POST", `/v1/agents/${AGENT_ID}/tasks`, {
          body: {
            admissionKey: "ak-2",
            kind: "digest",
            payload: {},
            taskProfile: { model: "gpt-4o", allowedTools: ["everything"] },
          },
        }),
      ),
      400,
      "invalid_request",
    );
    await expectProblem(
      await f.handler(
        request("POST", `/v1/agents/${AGENT_ID}/tasks`, {
          body: {
            admissionKey: "ak-2",
            kind: "digest",
            payload: {},
            projectSources: [{ id: "s", kind: "vault", units: [] }],
          },
        }),
      ),
      400,
      "invalid_request",
    );
  });

  it("passes the bounded event-page query to the port and answers with its page", async () => {
    const calls: PortCall[] = [];
    const seen: SeenAuthorization[] = [];
    const f = await createHandlerFixture(teammateDeps(calls, seen));
    const res = await f.handler(
      request("GET", `/v1/agent-tasks/${TASK_ID}/events?after=0&limit=10`),
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ events: [] });
    const call = calls.find((c) => c.method === "listAgentTaskEvents");
    expect(call?.args[0]).toBe(TASK_ID);
    expect(call?.args[1]).toEqual({ after: 0, limit: 10 });
    // `after` is a seq: 0 is legal (the `readPositiveInt` floor would
    // refuse the first page of a log that starts at 0).
    const negative = await f.handler(
      request("GET", `/v1/agent-tasks/${TASK_ID}/events?after=-1`),
    );
    await expectProblem(negative, 400, "invalid_request");
  });
});

/** One route call with the body that operation's validator admits. */
async function handlerCall(
  f: { handler: (req: Request) => Promise<Response> },
  operation: RestOperation,
): Promise<Response> {
  const body = bodyFor(operation);
  return f.handler(
    request(REST_ROUTES[operation].method, teammatePath(operation), {
      ...(body === undefined ? {} : { body }),
    }),
  );
}
