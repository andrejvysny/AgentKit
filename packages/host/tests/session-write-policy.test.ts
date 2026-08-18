import { describe, expect, it } from "bun:test";
import { RISK_RANK, SessionWritePolicy, writeAllowanceKey } from "../src/index.js";
import { createTestClock } from "./fakes.js";

const QUERY = {
  chatId: "chat-1",
  toolName: "write_items",
  proposalKind: "items.write",
  risk: "medium",
} as const;

describe("SessionWritePolicy", () => {
  it("defaults to confirming writes and denies without an allowance", () => {
    const policy = new SessionWritePolicy();
    expect(policy.mode()).toBe("auto_readonly_confirm_writes");
    expect(policy.isAutoApplyAllowed(QUERY)).toBe(false);
  });

  it("honours an allowance up to its risk ceiling and no further", () => {
    const policy = new SessionWritePolicy({ clock: createTestClock() });
    policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      maxRisk: "medium",
    });
    expect(policy.isAutoApplyAllowed({ ...QUERY, risk: "low" })).toBe(true);
    expect(policy.isAutoApplyAllowed({ ...QUERY, risk: "medium" })).toBe(true);
    expect(policy.isAutoApplyAllowed({ ...QUERY, risk: "high" })).toBe(false);
    expect(policy.isAutoApplyAllowed({ ...QUERY, risk: "destructive" })).toBe(
      false,
    );
  });

  it("scopes an allowance to its chat, tool and kind", () => {
    const policy = new SessionWritePolicy();
    policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      maxRisk: "destructive",
    });
    expect(policy.isAutoApplyAllowed(QUERY)).toBe(true);
    expect(policy.isAutoApplyAllowed({ ...QUERY, chatId: "chat-2" })).toBe(
      false,
    );
    expect(policy.isAutoApplyAllowed({ ...QUERY, toolName: "other" })).toBe(
      false,
    );
    expect(
      policy.isAutoApplyAllowed({ ...QUERY, proposalKind: "items.delete" }),
    ).toBe(false);
  });

  it("confirm_all_writes overrides every allowance without clearing it", () => {
    const policy = new SessionWritePolicy();
    policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      maxRisk: "destructive",
    });
    policy.setMode("confirm_all_writes");
    expect(policy.isAutoApplyAllowed(QUERY)).toBe(false);
    // Switching back restores the grant that was never revoked.
    policy.setMode("auto_readonly_confirm_writes");
    expect(policy.isAutoApplyAllowed(QUERY)).toBe(true);
  });

  it("auto_all allows everything, allowance or not", () => {
    const policy = new SessionWritePolicy({ mode: "auto_all" });
    expect(policy.isAutoApplyAllowed({ ...QUERY, risk: "destructive" })).toBe(
      true,
    );
  });

  it("lists and revokes allowances, scoped to the owning chat", () => {
    const policy = new SessionWritePolicy();
    const allowance = policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      maxRisk: "low",
    });
    policy.allow({
      chatId: "chat-2",
      toolName: "write_items",
      proposalKind: "items.write",
      maxRisk: "low",
    });
    expect(allowance.key).toBe(
      writeAllowanceKey("chat-1", "write_items", "items.write"),
    );
    expect(policy.list("chat-1")).toHaveLength(1);

    // Another chat cannot revoke this grant by guessing its key.
    policy.revoke("chat-9", allowance.key);
    expect(policy.list("chat-1")).toHaveLength(1);
    policy.revoke("chat-1", allowance.key);
    expect(policy.list("chat-1")).toHaveLength(0);
    expect(policy.list("chat-2")).toHaveLength(1);
  });

  it("ranks risk low < medium < high < destructive", () => {
    expect(RISK_RANK.low).toBeLessThan(RISK_RANK.medium);
    expect(RISK_RANK.medium).toBeLessThan(RISK_RANK.high);
    expect(RISK_RANK.high).toBeLessThan(RISK_RANK.destructive);
    expect(Object.isFrozen(RISK_RANK)).toBe(true);
  });
});
