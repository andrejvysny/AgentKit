import { describe, expect, it } from "bun:test";
import {
  ACTION_ID_GUIDANCE,
  isValidActionId,
  normalizeActionId,
} from "../src/proposals/action-id.js";

describe("isValidActionId", () => {
  it("accepts <verb>_<key>_<scopeId> shapes", () => {
    expect(isValidActionId("create_item-42_scope-7")).toBe(true);
    expect(isValidActionId("place_R1_design-1")).toBe(true);
    expect(isValidActionId("link_a.b__c.d_scope1")).toBe(true);
    expect(isValidActionId("update_x_ABC123")).toBe(true);
  });

  it("rejects ids that carry no verb/key/scope structure", () => {
    expect(isValidActionId("nope")).toBe(false);
    expect(isValidActionId("")).toBe(false);
    expect(isValidActionId("Create_item_scope")).toBe(false); // verb must be lowercase
    expect(isValidActionId("create__scope")).toBe(false); // empty key
    expect(isValidActionId("create_item_scope!")).toBe(false); // scope charset
  });
});

describe("normalizeActionId", () => {
  it("returns the trimmed id and warns about nothing when it is valid", () => {
    const warnings: string[] = [];
    expect(normalizeActionId("  create_item_scope-1  ", warnings)).toBe(
      "create_item_scope-1",
    );
    expect(warnings).toEqual([]);
  });

  it("treats an absent id as 'not idempotent', silently", () => {
    const warnings: string[] = [];
    expect(normalizeActionId(undefined, warnings)).toBeUndefined();
    expect(normalizeActionId(null, warnings)).toBeUndefined();
    expect(normalizeActionId("   ", warnings)).toBeUndefined();
    expect(warnings).toEqual([]);
  });

  it("degrades a malformed id to non-idempotent with a warning, never a throw", () => {
    const warnings: string[] = [];
    expect(normalizeActionId("garbage", warnings)).toBeUndefined();
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain("garbage");
    expect(warnings[0]).toContain("not idempotent");
  });

  it("warns on a non-string id (models send numbers and objects)", () => {
    const warnings: string[] = [];
    expect(normalizeActionId(42, warnings)).toBeUndefined();
    expect(normalizeActionId({ id: "x" }, warnings)).toBeUndefined();
    expect(warnings).toHaveLength(2);
    expect(warnings[0]).toContain("non-string");
  });
});

describe("ACTION_ID_GUIDANCE", () => {
  it("explains the safe-no-op property and stays host-neutral", () => {
    expect(ACTION_ID_GUIDANCE).toContain("action_id");
    expect(ACTION_ID_GUIDANCE).toContain("safe no-op");
    expect(ACTION_ID_GUIDANCE).toContain("scopeId");
    // No host/domain vocabulary leaked in from the system it was extracted from.
    expect(ACTION_ID_GUIDANCE.toLowerCase()).not.toContain("design");
    expect(ACTION_ID_GUIDANCE.toLowerCase()).not.toContain("schematic");
  });
});
