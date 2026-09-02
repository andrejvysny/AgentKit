/**
 * The staged-write routes over a real `ProposalService`.
 *
 * The property worth a socket-free test is the one the DTO makes easy to get
 * wrong: `apply` is keyed by the CLIENT's `operationId`, so replaying the same
 * request must return the recorded outcome and must not touch the world twice.
 */
import { describe, expect, it } from "bun:test";
import type { ProposalDto } from "@agentkit/contracts";
import {
  ProposalService,
  SessionWritePolicy,
  TaskService,
  TurnRunner,
  defaultClock,
  defaultIds,
  type ApplyOutcome,
  type ApplyProposalInput,
  type ProposalApplier,
} from "@agentkit/host";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { MockProviderClient } from "@agentkit/testing";
import { createRestHandler, type RestFetchHandler } from "../src/index.js";
import { InertTaskRunner, request } from "./support/fixture.js";

const CHAT_ID = "chat-proposals";

/** Counts the applies that reached the world — the replay guard's witness. */
class CountingApplier implements ProposalApplier {
  calls = 0;
  private readonly ledger = new Map<string, ApplyOutcome>();

  async apply(input: ApplyProposalInput): Promise<ApplyOutcome> {
    this.calls += 1;
    const outcome: ApplyOutcome = {
      status: "applied",
      appliedOps: input.proposal.operations.length,
      failedOps: [],
    };
    this.ledger.set(input.operationId, outcome);
    return outcome;
  }

  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    return this.ledger.get(operationId) ?? null;
  }
}

interface Fixture {
  store: MemoryAssistantStore;
  applier: CountingApplier;
  handler: RestFetchHandler;
}

async function fixture(): Promise<Fixture> {
  const store = new MemoryAssistantStore();
  const runner = new InertTaskRunner();
  const applier = new CountingApplier();
  const proposals = new ProposalService({
    store,
    applier,
    policy: new SessionWritePolicy({
      mode: "auto_readonly_confirm_writes",
      clock: defaultClock,
    }),
    clock: defaultClock,
    ids: defaultIds,
  });
  const provider = new MockProviderClient();
  const handler = createRestHandler({
    store,
    turns: new TurnRunner({
      store,
      taskRunner: runner,
      providerFactory: () => provider,
      contributors: [],
      clock: defaultClock,
      ids: defaultIds,
    }),
    tasks: new TaskService({
      store,
      taskRunner: runner,
      ids: defaultIds,
      clock: defaultClock,
    }),
    proposals,
  });
  await store.conversations.createChat({ id: CHAT_ID });
  return { store, applier, handler };
}

async function stage(store: MemoryAssistantStore, id: string): Promise<void> {
  await store.proposals.create({
    id,
    chatId: CHAT_ID,
    scopeKey: CHAT_ID,
    toolName: "notes_append",
    kind: "notes.append",
    risk: "medium",
    envelope: { summary: "Append a note" },
    operations: [{ op: "append", text: "hello" }],
    warnings: [],
    truncated: false,
    createdAt: defaultClock.nowIso(),
  });
}

describe("proposal routes", () => {
  it("(a) approves, applies, and replays one apply exactly once", async () => {
    const { store, applier, handler } = await fixture();
    await stage(store, "prp-1");

    const approved = await handler(
      request("POST", "/v1/proposals/prp-1/approve", {
        body: { reason: "looks right" },
      }),
    );
    expect(approved.status).toBe(200);
    const decided = (await approved.json()) as ProposalDto;
    expect(decided.status).toBe("approved");
    // A decision that arrived over HTTP came from a person, never a policy.
    expect(decided.decision?.actor).toBe("user");
    expect(decided.decision?.reason).toBe("looks right");
    expect(decided.decision?.policyId).toBeUndefined();

    const apply = (): Promise<Response> =>
      handler(
        request("POST", "/v1/proposals/prp-1/apply", {
          body: { operationId: "op-1" },
        }),
      );

    const first = (await (await apply()).json()) as ProposalDto;
    expect(first.status).toBe("applied");
    // The claim instant reaches the client, and is the record's own: it is what
    // a reviewer reads as "the apply started here", and the only stamp the
    // reconcile window can be explained by.
    expect(first.claimedAt).toBeDefined();
    expect(first.claimedAt).toBe(
      (await store.proposals.get("prp-1"))?.claimedAt,
    );
    expect(first.outcome).toEqual({
      status: "applied",
      appliedOps: 1,
      failedOps: [],
    });

    const replay = (await (await apply()).json()) as ProposalDto;
    expect(replay).toEqual(first);
    expect(applier.calls).toBe(1);
  });

  it("(b) rejects, and refuses a second decision with 409", async () => {
    const { store, handler } = await fixture();
    await stage(store, "prp-2");

    const rejected = (await (
      await handler(
        request("POST", "/v1/proposals/prp-2/reject", {
          body: { reason: "no" },
        }),
      )
    ).json()) as ProposalDto;
    expect(rejected.status).toBe("rejected");

    const again = await handler(
      request("POST", "/v1/proposals/prp-2/approve", { body: {} }),
    );
    expect(again.status).toBe(409);
    expect(again.headers.get("content-type")).toBe("application/problem+json");
    expect(((await again.json()) as { code: string }).code).toBe(
      "invalid_proposal_transition",
    );
  });

  it("(c) requires the client's operationId on apply", async () => {
    const { store, handler } = await fixture();
    await stage(store, "prp-3");
    const res = await handler(
      request("POST", "/v1/proposals/prp-3/apply", { body: {} }),
    );
    expect(res.status).toBe(400);
    expect(((await res.json()) as { code: string }).code).toBe(
      "invalid_request",
    );
  });

  it("(d) 404s an unknown proposal", async () => {
    const { handler } = await fixture();
    const res = await handler(
      request("POST", "/v1/proposals/nope/approve", { body: {} }),
    );
    expect(res.status).toBe(404);
    expect(((await res.json()) as { code: string }).code).toBe("not_found");
  });
});
