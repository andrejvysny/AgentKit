// A seeded random walk over a chat's tree, asserting the shape invariant after
// EVERY step.
//
// Why this exists alongside the hand-written branching tests: those tests each
// name one rule and check it on one fixture, which is exactly what they should
// do — and exactly why they cannot catch the failure this file is aimed at. The
// invariant "the active messages of a chat form one chain from a root to a
// CHILDLESS leaf" is not a property of any single operation; it is a property of
// every SEQUENCE of them, and the sequences that break it are the ones nobody
// thought to write down (a chain append onto a branch abandoned four steps ago,
// a switch back onto a node whose subtree grew while it was off-path). A driver
// that composes the operations at random finds those; a fixture cannot.
//
// It is a permanent part of the conformance suite rather than a one-off script
// for the same reason `task-schedule-driver.ts` is: an adapter added next year
// gets graded on the same walks, and a regression in the shared tree arithmetic
// shows up as a named seed and a step number instead of as a bug report from
// production.
//
// SEEDED, so a failure is reproducible: the same seed replays the same walk on
// both adapters, and the failing step prints the seed to re-run with. The RNG is
// `task-schedule-driver`'s `mulberry32`, reused rather than re-inlined so the
// two drivers cannot disagree about what "seed 7" means.
//
// FRAMEWORK-NEUTRAL, same rules as the rest of this package: no runner import,
// every `@agentkit/host` import is `import type`, and error assertions match on
// the `code` string rather than on `instanceof`.
import type { MessageRecord } from "@agentkit/host";
import type {
  AssistantStoreConformanceHarness,
  AssistantStoreConformanceTestApi,
} from "./conformance-support.js";
import { createRng, type Rng } from "./task-schedule-driver.js";

export interface ConversationTreeInvariantOptions {
  create: () => Promise<AssistantStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/**
 * Fixed rather than random, so a run of this suite is the same run every time.
 * Five is enough to cover the interesting orderings and cheap enough that
 * nobody is tempted to skip the file.
 */
const SEEDS = [1, 7, 23, 101, 4242] as const;

/** Long enough for branches to be abandoned, grown, and returned to. */
const STEPS_PER_SEED = 60;

const ROLES: readonly MessageRecord["role"][] = [
  "user",
  "assistant",
  "tool",
  "system",
];

/**
 * The structure of everything written so far, mirrored in the test.
 *
 * Only the IMMUTABLE half is mirrored — id, parent, depth, order key. The
 * `active` flag is deliberately not: it is the thing under test, and a mirror
 * of it would just be the driver grading its own model of the store instead of
 * the store. Liveness comes from `listMessages` on every check.
 */
interface Node {
  id: string;
  parentMessageId?: string;
  depth: number;
  orderKey: number;
}

/** One recorded step, for the message a failure prints. */
type StepKind =
  | "append"
  | "branch"
  | "chain-leaf"
  | "chain-off-path"
  | "chain-superseded"
  | "switch";

export function describeConversationTreeInvariants(
  options: ConversationTreeInvariantOptions,
): void {
  const { create, test } = options;
  const { describe, it, expect } = test;

  describe("conversation tree invariants (seeded random walk)", () => {
    for (const seed of SEEDS) {
      it(`holds the active-path invariant across ${STEPS_PER_SEED} random operations — seed ${seed}`, async () => {
        const { store, close } = await create();
        try {
          await walk(store, createRng(seed), seed, expect);
        } finally {
          close?.();
        }
      });
    }
  });
}

async function walk(
  store: AssistantStoreConformanceHarness["store"],
  rng: Rng,
  seed: number,
  expect: AssistantStoreConformanceTestApi["expect"],
): Promise<void> {
  const chat = await store.conversations.createChat({});
  const nodes: Node[] = [];
  /** Children by parent id — what proves the path's last message is a leaf. */
  const childCount = new Map<string, number>();

  const record = (message: MessageRecord): void => {
    nodes.push({
      id: message.id,
      ...(message.parentMessageId === undefined
        ? {}
        : { parentMessageId: message.parentMessageId }),
      depth: message.depth,
      orderKey: message.orderKey,
    });
    if (message.parentMessageId !== undefined) {
      childCount.set(
        message.parentMessageId,
        (childCount.get(message.parentMessageId) ?? 0) + 1,
      );
    }
  };

  for (let step = 0; step < STEPS_PER_SEED; step += 1) {
    const path = await store.conversations.listMessages(chat.id);
    const kind = pickStep(rng, nodes.length, path.length);
    const where = `seed ${seed}, step ${step} (${kind})`;
    const role = rng.pick(ROLES);
    const leaf = path.at(-1);
    const offPath = nodes.filter(
      (node) => !path.some((live) => live.id === node.id),
    );

    let returned: MessageRecord[] | undefined;
    switch (kind) {
      case "append": {
        // The pre-branching append: no parent named, so the store hangs it off
        // whatever the active leaf currently is.
        record(
          await store.conversations.appendMessage({
            chatId: chat.id,
            role,
            content: where,
          }),
        );
        break;
      }
      case "branch": {
        // Any node at all, including one deep inside an abandoned subtree: a
        // branching append is also a path switch, and switching ONTO a branch
        // that has grown since it was left is the case the descent rules exist
        // for.
        const parent = rng.pick(nodes);
        record(
          await store.conversations.appendMessage({
            chatId: chat.id,
            role,
            content: where,
            parentMessageId: parent.id,
          }),
        );
        break;
      }
      case "chain-leaf": {
        // A run writing its next record with nobody having touched the path:
        // inherits `active: true`, extends the same chain.
        record(
          await store.conversations.appendMessage({
            chatId: chat.id,
            role,
            content: where,
            parentMessageId: leaf?.id ?? "",
            activate: false,
          }),
        );
        break;
      }
      case "chain-superseded": {
        // The third chain case: the parent is still ON the path but is no
        // longer the end of it, because a bare append landed between two of
        // the run's writes. Inheriting `active` there would give one message
        // two active children, so the record goes off-path instead — the same
        // outcome a branch switch produces, reached a different way.
        const before = path.map((message) => message.id);
        const parent = rng.pick(path.slice(0, -1));
        record(
          await store.conversations.appendMessage({
            chatId: chat.id,
            role,
            content: where,
            parentMessageId: parent.id,
            activate: false,
          }),
        );
        expect(
          (await store.conversations.listMessages(chat.id)).map((m) => m.id),
        ).toEqual(before);
        break;
      }
      case "chain-off-path": {
        // The same run one branch switch later: the chain continues on the
        // branch it started on, inherits `active: false`, and the live path
        // must not notice.
        const before = path.map((message) => message.id);
        record(
          await store.conversations.appendMessage({
            chatId: chat.id,
            role,
            content: where,
            parentMessageId: rng.pick(offPath).id,
            activate: false,
          }),
        );
        expect(
          (await store.conversations.listMessages(chat.id)).map((m) => m.id),
        ).toEqual(before);
        break;
      }
      case "switch": {
        returned = await store.conversations.activatePath(rng.pick(nodes).id);
        break;
      }
    }

    const after = await store.conversations.listMessages(chat.id);
    if (returned !== undefined) {
      // What a switch answers with is what the chat now reads as — the whole
      // reason `activatePath` returns a path instead of nothing.
      expect(returned.map((m) => m.id)).toEqual(after.map((m) => m.id));
    }
    assertOneActiveChain(after, childCount, where, expect);
  }
}

/**
 * Which operation this step performs.
 *
 * The three chain-append variants are separated because their preconditions
 * are: `chain-leaf` needs a path to chain onto, `chain-superseded` needs a path
 * with something above its leaf, `chain-off-path` needs a node off the path.
 * Rolling a kind whose precondition does not hold and
 * silently substituting another would make the seed's walk depend on the
 * store's state, and a "reproducible" seed that replays differently on two
 * adapters is worse than no seed at all — so the choice is narrowed to the
 * kinds that are legal BEFORE the roll, from counts both adapters agree on.
 */
function pickStep(rng: Rng, nodeCount: number, pathLength: number): StepKind {
  if (nodeCount === 0) return "append";
  const kinds: StepKind[] = ["append", "branch", "switch"];
  if (pathLength > 0) kinds.push("chain-leaf");
  if (pathLength > 1) kinds.push("chain-superseded");
  if (nodeCount > pathLength) kinds.push("chain-off-path");
  return rng.pick(kinds);
}

/**
 * The invariant, in full: what `listMessages` returns is ONE chain from a root
 * to a childless leaf, with `depth` and `orderKey` agreeing along it.
 *
 * `listMessages` reports every active message in the chat, not a walk — so a
 * result that is a well-formed root-to-leaf chain IS the statement that the
 * active set is exactly that chain. A second active leaf, an orphan left active
 * by a half-applied switch, or a stale flag under an abandoned branch all show
 * up here as a broken link or a wrong depth rather than as a silent extra row.
 *
 * One `toEqual` over a whole report rather than six separate assertions, so a
 * failure prints the seed, the step and every property at once — the walk that
 * produced it is only reproducible if the message says which walk it was.
 */
function assertOneActiveChain(
  path: readonly MessageRecord[],
  childCount: ReadonlyMap<string, number>,
  where: string,
  expect: AssistantStoreConformanceTestApi["expect"],
): void {
  const leaf = path[path.length - 1];
  expect({
    where,
    nonEmpty: path.length > 0,
    parents: path.map((message) => message.parentMessageId),
    depths: path.map((message) => message.depth),
    active: path.map((message) => message.active),
    // `orderKey` rises with depth along any chain — the property the
    // `afterOrderKey` cursor is built on.
    orderKeysRise: path.every(
      (message, index) =>
        index === 0 || message.orderKey > (path[index - 1]?.orderKey ?? 0),
    ),
    // The leaf is a LEAF, counting children the store never showed us as well
    // as ones it did: an active chain that stopped short of one would mean the
    // descent gave up partway, and `listMessages` would be reporting a
    // conversation with an unread continuation hanging off its end.
    leafChildren: leaf === undefined ? -1 : (childCount.get(leaf.id) ?? 0),
  }).toEqual({
    where,
    nonEmpty: true,
    parents: [undefined, ...path.slice(0, -1).map((message) => message.id)],
    depths: path.map((_, index) => index),
    active: path.map(() => true),
    orderKeysRise: true,
    leafChildren: 0,
  });
}
