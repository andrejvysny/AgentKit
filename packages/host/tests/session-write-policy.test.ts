import { describe, expect, it } from "bun:test";
import {
  RISK_RANK,
  SessionWritePolicy,
  writeAllowanceKey,
} from "../src/index.js";
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

  // C4: the scope comes from MODEL-SUPPLIED tool input, so a grant that
  // ignored it turns "yes, edit this document" into a standing yes for every
  // document the tool can reach.
  describe("scoped allowances", () => {
    it("confines a scoped grant to the scope it was given for", () => {
      const policy = new SessionWritePolicy();
      policy.allow({
        chatId: "chat-1",
        toolName: "write_items",
        proposalKind: "items.write",
        scopeKey: "doc-a",
        maxRisk: "destructive",
      });
      expect(policy.isAutoApplyAllowed({ ...QUERY, scopeKey: "doc-a" })).toBe(
        true,
      );
      expect(policy.isAutoApplyAllowed({ ...QUERY, scopeKey: "doc-b" })).toBe(
        false,
      );
      // And a query that names no scope does not inherit a scoped grant.
      expect(policy.isAutoApplyAllowed(QUERY)).toBe(false);
    });

    it("keeps an UNSCOPED grant covering every scope — what every grant meant before", () => {
      const policy = new SessionWritePolicy();
      policy.allow({
        chatId: "chat-1",
        toolName: "write_items",
        proposalKind: "items.write",
        maxRisk: "destructive",
      });
      expect(policy.isAutoApplyAllowed({ ...QUERY, scopeKey: "doc-a" })).toBe(
        true,
      );
      expect(policy.isAutoApplyAllowed({ ...QUERY, scopeKey: "doc-b" })).toBe(
        true,
      );
    });

    it("keeps a scoped and an unscoped grant apart, and applies each ceiling", () => {
      const policy = new SessionWritePolicy();
      policy.allow({
        chatId: "chat-1",
        toolName: "write_items",
        proposalKind: "items.write",
        maxRisk: "low",
      });
      const scoped = policy.allow({
        chatId: "chat-1",
        toolName: "write_items",
        proposalKind: "items.write",
        scopeKey: "doc-a",
        maxRisk: "destructive",
      });
      expect(policy.list("chat-1")).toHaveLength(2);
      expect(scoped.scopeKey).toBe("doc-a");
      // The narrow grant wins where it applies…
      expect(
        policy.isAutoApplyAllowed({
          ...QUERY,
          scopeKey: "doc-a",
          risk: "destructive",
        }),
      ).toBe(true);
      // …and elsewhere only the broad, low-risk one does.
      expect(
        policy.isAutoApplyAllowed({
          ...QUERY,
          scopeKey: "doc-b",
          risk: "destructive",
        }),
      ).toBe(false);
      expect(
        policy.isAutoApplyAllowed({ ...QUERY, scopeKey: "doc-b", risk: "low" }),
      ).toBe(true);
    });

    // C14: every member of the key is caller or model data, so a `:`-joined
    // string lets two different grants collide on one key.
    it("escapes separators inside the key's members", () => {
      expect(writeAllowanceKey("doc:1", "t", "edits")).not.toBe(
        writeAllowanceKey("doc", "t", "1:edits"),
      );
      // "no scope" and "a scope literally named the empty string" are not the
      // same grant either.
      expect(writeAllowanceKey("c", "t", "k")).not.toBe(
        writeAllowanceKey("c", "t", "k", ""),
      );
    });
  });

  it("ranks risk low < medium < high < destructive", () => {
    expect(RISK_RANK.low).toBeLessThan(RISK_RANK.medium);
    expect(RISK_RANK.medium).toBeLessThan(RISK_RANK.high);
    expect(RISK_RANK.high).toBeLessThan(RISK_RANK.destructive);
    expect(Object.isFrozen(RISK_RANK)).toBe(true);
  });
});
