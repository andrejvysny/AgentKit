import { describe, expect, it } from "bun:test";
import {
  activationSetOf,
  activeLeafOf,
  activePathOf,
  childrenOf,
  forkPrefixOf,
  forkedChatTitle,
  nextBranchIndex,
  planForkedMessages,
  siblingsOf,
  type MessageRecord,
} from "../src/index.js";

/**
 * These helpers are graded end-to-end by the store-conformance suite against
 * both adapters; this file is the unit half, for the cases a store cannot easily
 * be driven into — a chat whose flags are inconsistent, a parent cycle, a
 * `branchIndex` sequence with a hole in it. Those are the shapes that decide
 * whether a corrupt chat returns wrong or hangs.
 */
function record(
  overrides: Partial<MessageRecord> & { id: string },
): MessageRecord {
  return {
    chatId: "chat-1",
    role: "user",
    content: overrides.id,
    orderKey: 0,
    depth: 0,
    branchIndex: 0,
    active: false,
    metadata: {},
    createdAt: "2026-01-01T00:00:00.000Z",
    ...overrides,
  };
}

/**
 * ```
 * u1 (0)
 * ├── a1 (1) branchIndex 0
 * │   └── u2 (2)
 * └── a2 (1) branchIndex 1   ← active, with u3 under it
 *     └── u3 (2)
 * ```
 */
function tree(): MessageRecord[] {
  return [
    record({ id: "u1", orderKey: 1, depth: 0, active: true }),
    record({ id: "a1", orderKey: 2, depth: 1, parentMessageId: "u1" }),
    record({ id: "u2", orderKey: 3, depth: 2, parentMessageId: "a1" }),
    record({
      id: "a2",
      orderKey: 4,
      depth: 1,
      branchIndex: 1,
      parentMessageId: "u1",
      active: true,
    }),
    record({
      id: "u3",
      orderKey: 5,
      depth: 2,
      parentMessageId: "a2",
      active: true,
    }),
  ];
}

const ids = (records: readonly MessageRecord[]) => records.map((r) => r.id);

describe("activePathOf / activeLeafOf", () => {
  it("returns the active records in (depth, orderKey) order", () => {
    expect(ids(activePathOf(tree()))).toEqual(["u1", "a2", "u3"]);
    expect(activeLeafOf(tree())?.id).toBe("u3");
  });

  it("orders by depth first, so append order cannot outrank tree position", () => {
    // A record written LATER but shallower still comes first — the case that
    // separates path ordering from plain orderKey ordering.
    const records = [
      record({ id: "deep", orderKey: 1, depth: 5, active: true }),
      record({ id: "shallow", orderKey: 9, depth: 0, active: true }),
    ];
    expect(ids(activePathOf(records))).toEqual(["shallow", "deep"]);
  });

  it("has no leaf on an empty chat or one with nothing active", () => {
    expect(activeLeafOf([])).toBe(undefined);
    expect(activeLeafOf([record({ id: "x" })])).toBe(undefined);
  });
});

describe("childrenOf / siblingsOf / nextBranchIndex", () => {
  it("finds a parent's children in branchIndex order, and the roots for undefined", () => {
    expect(ids(childrenOf(tree(), "u1"))).toEqual(["a1", "a2"]);
    expect(ids(childrenOf(tree(), undefined))).toEqual(["u1"]);
    expect(ids(childrenOf(tree(), "u3"))).toEqual([]);
  });

  it("includes the record itself in its siblings", () => {
    const records = tree();
    const a1 = records.find((r) => r.id === "a1");
    expect(ids(siblingsOf(records, a1 as MessageRecord))).toEqual(["a1", "a2"]);
  });

  it("takes the next index past the HIGHEST used, not the sibling count", () => {
    expect(nextBranchIndex(tree(), "u1")).toBe(2);
    expect(nextBranchIndex(tree(), "u3")).toBe(0);
    // A hole in the sequence (index 1 removed) must not hand out an index a
    // client already knows as a different message.
    const holed = [
      record({ id: "p" }),
      record({ id: "c0", parentMessageId: "p", branchIndex: 0 }),
      record({ id: "c2", parentMessageId: "p", branchIndex: 2 }),
    ];
    expect(nextBranchIndex(holed, "p")).toBe(3);
  });
});

describe("activationSetOf", () => {
  it("takes ancestors, the message, and a descent preferring the active child", () => {
    expect([...activationSetOf(tree(), "u1")].sort()).toEqual([
      "a2",
      "u1",
      "u3",
    ]);
  });

  it("falls back to the lowest branchIndex when no child is active", () => {
    expect([...activationSetOf(tree(), "a1")].sort()).toEqual([
      "a1",
      "u1",
      "u2",
    ]);
  });

  it("is empty for an id the chat does not have", () => {
    expect(activationSetOf(tree(), "nope").size).toBe(0);
  });

  it("terminates on a parent cycle instead of hanging", () => {
    // Unreachable through the port; reachable through hand-edited storage, and
    // a corrupt chat should fail to switch rather than fail to return.
    const cyclic = [
      record({ id: "a", parentMessageId: "b" }),
      record({ id: "b", parentMessageId: "a" }),
    ];
    expect([...activationSetOf(cyclic, "a")].sort()).toEqual(["a", "b"]);
  });
});

describe("forkPrefixOf", () => {
  it("cuts the active path at the fork point, inclusive", () => {
    expect(ids(forkPrefixOf(tree(), "chat-1", "a2"))).toEqual(["u1", "a2"]);
    expect(ids(forkPrefixOf(tree(), "chat-1", "u3"))).toEqual([
      "u1",
      "a2",
      "u3",
    ]);
  });

  it("rejects an id the chat does not have, and one that is off the path", () => {
    let notInChat: { code?: string; details?: Record<string, unknown> } = {};
    try {
      forkPrefixOf(tree(), "chat-1", "nope");
    } catch (err) {
      notInChat = err as typeof notInChat;
    }
    expect(notInChat.code).toBe("invalid_fork_point");
    expect(notInChat.details?.["reason"]).toBe("not_in_chat");

    let offPath: { code?: string; details?: Record<string, unknown> } = {};
    try {
      forkPrefixOf(tree(), "chat-1", "a1");
    } catch (err) {
      offPath = err as typeof offPath;
    }
    expect(offPath.code).toBe("invalid_fork_point");
    expect(offPath.details?.["reason"]).toBe("inactive_branch");
  });

  it("drops a placeholder from the middle of the prefix", () => {
    const records = [
      record({ id: "u1", orderKey: 1, depth: 0, active: true }),
      record({
        id: "ph",
        orderKey: 2,
        depth: 1,
        parentMessageId: "u1",
        active: true,
        metadata: { placeholder: true },
      }),
      record({
        id: "a1",
        orderKey: 3,
        depth: 2,
        parentMessageId: "ph",
        active: true,
        metadata: { placeholder: false },
      }),
    ];
    expect(ids(forkPrefixOf(records, "chat-1", "a1"))).toEqual(["u1", "a1"]);
  });

  it("allows forking FROM a placeholder, copying only what is above it", () => {
    const records = [
      record({ id: "u1", orderKey: 1, depth: 0, active: true }),
      record({
        id: "ph",
        orderKey: 2,
        depth: 1,
        parentMessageId: "u1",
        active: true,
        metadata: { placeholder: true },
      }),
    ];
    expect(ids(forkPrefixOf(records, "chat-1", "ph"))).toEqual(["u1"]);
  });
});

describe("planForkedMessages", () => {
  it("mints ids, re-links across gaps, recounts depth, and drops the placeholder flag", () => {
    let n = 0;
    const mintId = (): string => {
      n += 1;
      return `new-${n}`;
    };
    const prefix = [
      record({ id: "u1", depth: 0, metadata: { keep: 1 } }),
      // Its stored parent is a message the prefix does not contain — the shape a
      // dropped placeholder leaves behind.
      record({
        id: "a1",
        depth: 7,
        parentMessageId: "dropped",
        metadata: { internal: true, placeholder: false },
      }),
    ];
    const plans = planForkedMessages(prefix, mintId);
    expect(plans.map((p) => p.id)).toEqual(["new-1", "new-2"]);
    expect(plans.map((p) => p.parentMessageId)).toEqual([undefined, "new-1"]);
    expect(plans.map((p) => p.depth)).toEqual([0, 1]);
    expect(plans[0]?.metadata).toEqual({ keep: 1 });
    expect(plans[1]?.metadata).toEqual({ internal: true });
    // The source is not mutated on the way through.
    expect(prefix[1]?.metadata).toEqual({
      internal: true,
      placeholder: false,
    });
  });
});

describe("forkedChatTitle", () => {
  it("prefixes a title and leaves an absent one absent", () => {
    expect(forkedChatTitle("Design review")).toBe("Fork of Design review");
    expect(forkedChatTitle(undefined)).toBe(undefined);
  });
});
