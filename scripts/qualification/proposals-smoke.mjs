import assert from "node:assert/strict";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import {
  createProposalBuilderTool,
  ProposalService,
  SessionWritePolicy,
  defaultClock,
  defaultIds,
} from "agentkit/host";
function builder(store, service, policy, name, risk) {
  return createProposalBuilderTool({
    store,
    service,
    policy,
    ids: defaultIds,
    definition: {
      name,
      version: "1",
      effect: "write",
      capability: name,
      description: "Write a fixture value",
      inputSchema: {
        type: "object",
        properties: { action_id: { type: "string" } },
        additionalProperties: false,
      },
    },
    scopeKeyOf: () => "fixture-scope",
    async build() {
      return {
        kind: name,
        risk,
        operations: [{ fixture: true }],
        warnings: [],
        truncated: false,
      };
    },
  });
}

export function proposalFixture(store, path) {
  const ledgerPath = `${path}.applied.json`;
  const ledger = existsSync(ledgerPath)
    ? JSON.parse(readFileSync(ledgerPath, "utf8"))
    : {};
  const policy = new SessionWritePolicy({ clock: defaultClock });
  const service = new ProposalService({
    store,
    policy,
    clock: defaultClock,
    ids: defaultIds,
    applier: {
      async apply({ operationId, proposal }) {
        assert.equal(proposal.risk, "low");
        assert.ok(!ledger[operationId]);
        const outcome = {
          status: "applied",
          appliedOps: proposal.operations.length,
          failedOps: [],
        };
        ledger[operationId] = outcome;
        writeFileSync(ledgerPath, JSON.stringify(ledger));
        return outcome;
      },
      async getOutcome(operationId) {
        return ledger[operationId] ?? null;
      },
    },
  });
  return {
    service,
    policy,
    ledger,
    tools: [
      builder(store, service, policy, "write_items", "low"),
      builder(store, service, policy, "destroy_items", "destructive"),
    ],
  };
}
export async function assertProposals(fixture, store, chatId, reject = true) {
  const applied = await store.proposals.getByActionId(
    "fixture-scope",
    "write_item_fixture",
  );
  const destructive = await store.proposals.getByActionId(
    "fixture-scope",
    "destroy_item_fixture",
  );
  assert.equal(applied.status, "applied");
  if (reject) {
    assert.equal(destructive.status, "pending");
    await fixture.service.reject({
      proposalId: destructive.id,
      reason: "Fixture denies destructive write",
    });
  }
  assert.equal((await store.proposals.get(destructive.id)).status, "rejected");
  assert.equal(applied.chatId, chatId);
  await fixture.service.apply({
    proposalId: applied.id,
    operationId: applied.operationId,
  });
  await assert.rejects(
    fixture.service.apply({
      proposalId: destructive.id,
      operationId: "denied-operation",
    }),
  );
  assert.equal(Object.keys(fixture.ledger).length, 1);
}
