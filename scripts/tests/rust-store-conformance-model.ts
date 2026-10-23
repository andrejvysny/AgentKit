/** Language-neutral, seeded schedules. Expectations are explicit protocol states. */
export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };
export interface ImportMessage {
  id: string;
  role: "user" | "assistant" | "tool";
  content: string;
  parentId?: string;
  active: boolean;
  internal?: boolean;
}
export interface EventInput {
  seq: number;
  id: string;
  type?: string;
  timestamp?: string;
  contractVersion?: string;
}
export type Operation =
  | {
      op: "submit";
      id: string;
      scope: string;
      priority: number;
      kind?: string;
      depends?: string[];
      parent?: string;
      available?: number;
      exclusive?: boolean;
    }
  | { op: "claim"; slot: string; busy?: string[]; kinds?: string[] }
  | { op: "append"; slot: string; events: EventInput[] }
  | { op: "settle"; slot: string; status: "completed" | "failed" | "cancelled" }
  | { op: "cancel" | "resume"; id: string }
  | { op: "recover" }
  | { op: "checkpoint"; ids: string[] }
  | { op: "import"; chatId: string; messages: ImportMessage[] }
  | { op: "readImport"; chatId: string; ids: string[] };
export interface Step {
  at: number;
  action: Operation;
  expected: Json;
}
export interface Scenario {
  name: string;
  seed: number;
  steps: Step[];
}
export interface Vectors {
  formatVersion: number;
  seeds: number[];
  scenarios: Scenario[];
}

interface TaskState {
  taskId: string;
  scopeId: string;
  status: string;
  priority: number;
  dependsOn: string[];
  parentTaskId: string | null;
  attemptCount: number;
  poisonCount: number;
  events: { seq: number; id: string }[];
  nextSeq: number;
}
const ok = { ok: true };
const error = (code: string): Json => ({ error: code });

class Schedule {
  now = 1000;
  readonly steps: Step[] = [];
  readonly tasks = new Map<string, TaskState>();
  readonly slots = new Map<string, string>();
  add(action: Operation, expected: Json): void {
    this.steps.push({
      at: this.now,
      action,
      expected: structuredClone(expected),
    });
  }
  submit(
    id: string,
    priority: number,
    options: Partial<Extract<Operation, { op: "submit" }>> = {},
  ): void {
    const action: Extract<Operation, { op: "submit" }> = {
      op: "submit",
      id,
      scope: id,
      priority,
      ...options,
    };
    this.add(action, ok);
    this.tasks.set(id, {
      taskId: id,
      scopeId: action.scope,
      status: "queued",
      priority,
      dependsOn: action.depends ?? [],
      parentTaskId: action.parent ?? null,
      attemptCount: 0,
      poisonCount: 0,
      events: [],
      nextSeq: 0,
    });
  }
  claim(
    slot: string,
    id: string | null,
    options: Partial<Extract<Operation, { op: "claim" }>> = {},
  ): void {
    this.add({ op: "claim", slot, ...options }, id);
    if (id === null) return;
    this.slots.set(slot, id);
    const task = this.tasks.get(id)!;
    task.status = "running";
    task.attemptCount++;
  }
  settle(slot: string, status: "completed" | "failed" | "cancelled"): void {
    this.add({ op: "settle", slot, status }, ok);
    this.tasks.get(this.slots.get(slot)!)!.status = status;
  }
  cancel(id: string): void {
    this.add({ op: "cancel", id }, ok);
    this.tasks.get(id)!.status = "cancelled";
  }
  append(slot: string, events: EventInput[]): void {
    this.add({ op: "append", slot, events }, ok);
    const task = this.tasks.get(this.slots.get(slot)!)!;
    task.events.push(...events.map(({ seq, id }) => ({ seq, id })));
    task.nextSeq = events.at(-1)!.seq + 1;
  }
  checkpoint(): void {
    const ids = [...this.tasks.keys()].sort();
    this.add(
      { op: "checkpoint", ids },
      ids.map((id) => ({ ...this.tasks.get(id)! })) as Json,
    );
  }
}

/** xorshift32 only chooses priorities and FIFO identities; no host randomness. */
function random(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return state >>> 0;
  };
}

function scenario(seed: number): Scenario {
  const next = random(seed);
  const s = new Schedule();
  const priority = 2 + (next() % 20);
  const [first, second] = next() % 2 === 0 ? ["c", "d"] : ["d", "c"];
  queueSetup(s, priority, first!, second!);
  addBatchFaults(s);
  drainQueue(s, first!, second!);
  recoverCrashedTask(s);
  addGenericEvents(s);
  addImports(s);
  return { name: `seed-${seed}`, seed, steps: s.steps };
}

function addGenericEvents(s: Schedule): void {
  s.submit("numeric", 0);
  s.claim("numeric-owner", "numeric");
  s.add(
    {
      op: "append",
      slot: "numeric-owner",
      events: [{ seq: -1, id: "invalid-first" }],
    },
    error("seq_conflict"),
  );
  s.append("numeric-owner", [
    { seq: -0.5, id: "", type: "", timestamp: "", contractVersion: "" },
  ]);
  s.append("numeric-owner", [
    {
      seq: 0.5,
      id: "half",
      type: "generic.fraction",
      timestamp: "arbitrary timestamp",
      contractVersion: "future",
    },
  ]);
  for (const bad of [0.25, 0.75]) {
    s.add(
      {
        op: "append",
        slot: "numeric-owner",
        events: [
          { seq: 0.75, id: "partial" },
          { seq: bad, id: "bad" },
        ],
      },
      error("seq_conflict"),
    );
  }
  s.append("numeric-owner", [{ seq: 1e100, id: "wide" }]);
  s.checkpoint();
  s.settle("numeric-owner", "completed");
}

function queueSetup(
  s: Schedule,
  priority: number,
  first: string,
  second: string,
): void {
  s.submit("a", 1);
  s.submit("b", 100, { depends: ["a"], parent: "a" });
  s.submit(first!, priority);
  s.submit(second!, priority);
  s.submit("delayed", 300, { available: 1050 });
  s.add(
    {
      op: "submit",
      id: "unknown",
      scope: "unknown",
      priority: 0,
      depends: ["missing"],
    },
    error("unknown_dependency"),
  );
  s.add(
    { op: "submit", id: "a", scope: "a", priority: 1 },
    error("duplicate_task"),
  );
  s.add(
    { op: "submit", id: "exclusive", scope: "a", priority: 0, exclusive: true },
    error("chat_busy"),
  );
}

function addBatchFaults(s: Schedule): void {
  s.claim("root", "a", { busy: ["c", "d"] });
  s.append("root", [{ seq: 2, id: "a-2" }]);
  s.append("root", [
    { seq: 5, id: "a-5" },
    { seq: 9, id: "a-9" },
  ]);
  s.add(
    {
      op: "append",
      slot: "root",
      events: [
        { seq: 12, id: "partial" },
        { seq: 12, id: "bad-seq" },
      ],
    },
    error("seq_conflict"),
  );
  s.add(
    { op: "append", slot: "root", events: [{ seq: 12, id: "a-2" }] },
    error("seq_conflict"),
  );
  s.add(
    {
      op: "append",
      slot: "root",
      events: [
        { seq: 12, id: "partial" },
        { seq: 8, id: "regression" },
      ],
    },
    error("seq_conflict"),
  );
  s.checkpoint();
}

function drainQueue(s: Schedule, first: string, second: string): void {
  s.settle("root", "completed");
  s.claim("dependent", "b");
  s.settle("dependent", "failed");
  s.submit("failed-child", 200, { depends: ["b"] });
  s.claim("kind-filter", null, { kinds: ["other"] });
  s.claim("tie-first", first!);
  s.tasks.get("failed-child")!.status = "failed";
  s.settle("tie-first", "completed");
  s.claim("busy-filter", null, { busy: [second!] });
  s.claim("tie-second", second!);
  s.cancel("delayed");
  s.settle("tie-second", "completed");
  s.claim("empty", null);
  s.submit("cancel-root", 1);
  s.submit("cancel-child", 10, { depends: ["cancel-root"] });
  s.cancel("cancel-root");
  s.claim("cancel-cascade", null);
  s.tasks.get("cancel-child")!.status = "cancelled";
  s.checkpoint();
}

function recoverCrashedTask(s: Schedule): void {
  s.now = 1100;
  s.submit("crash", 0);
  s.claim("old", "crash");
  s.append("old", [{ seq: 0, id: "crash-0" }]);
  s.now = 1199;
  s.append("old", [{ seq: 1, id: "crash-1" }]);
  s.now = 1200;
  s.add(
    { op: "append", slot: "old", events: [{ seq: 2, id: "expired" }] },
    error("lease_lost"),
  );
  s.add({ op: "recover" }, 1);
  s.tasks.get("crash")!.status = "interrupted";
  s.tasks.get("crash")!.poisonCount++;
  s.checkpoint();
  s.claim("before-resume", null);
  s.add({ op: "resume", id: "crash" }, ok);
  s.tasks.get("crash")!.status = "queued";
  s.claim("new", "crash");
  s.add(
    { op: "append", slot: "old", events: [{ seq: 2, id: "stale" }] },
    error("lease_lost"),
  );
  s.add({ op: "settle", slot: "old", status: "failed" }, error("lease_lost"));
  s.append("new", [{ seq: 2, id: "crash-2" }]);
  s.settle("new", "completed");
  s.add({ op: "recover" }, 0);
  s.checkpoint();
}

const importedMessages: ImportMessage[] = [
  { id: "u1", role: "user", content: "question", active: true },
  {
    id: "a1",
    role: "assistant",
    content: "old",
    parentId: "u1",
    active: false,
  },
  {
    id: "t1",
    role: "tool",
    content: "old evidence",
    parentId: "a1",
    active: false,
    internal: true,
  },
  {
    id: "a2",
    role: "assistant",
    content: "new",
    parentId: "u1",
    active: true,
  },
  {
    id: "t2",
    role: "tool",
    content: "new evidence",
    parentId: "a2",
    active: true,
    internal: true,
  },
];
function addImports(s: Schedule): void {
  const messages = importedMessages;
  s.add({ op: "import", chatId: "imported", messages }, ok);
  const shape = [
    [1, 0, 0],
    [2, 1, 0],
    [3, 2, 0],
    [4, 1, 1],
    [5, 2, 0],
  ];
  const expected = messages.map((m, i) => ({
    id: m.id,
    role: m.role,
    content: m.content,
    parentId: m.parentId ?? null,
    active: m.active,
    metadata: (m.internal ? { internal: true } : {}) as Json,
    orderKey: shape[i]![0]!,
    depth: shape[i]![1]!,
    branchIndex: shape[i]![2]!,
  }));
  s.add(
    { op: "readImport", chatId: "imported", ids: messages.map((m) => m.id) },
    { exists: true, messages: expected },
  );
  const invalid: ImportMessage[] = [
    {
      id: "bad-child",
      role: "assistant",
      content: "child",
      parentId: "bad-parent",
      active: true,
    },
    { id: "bad-parent", role: "user", content: "parent", active: true },
  ];
  s.add(
    { op: "import", chatId: "invalid", messages: invalid },
    error("invalid_import"),
  );
  s.add(
    { op: "readImport", chatId: "invalid", ids: invalid.map((m) => m.id) },
    { exists: false, messages: [] },
  );
}

export function buildVectors(): Vectors {
  const seeds = [1, 7, 42, 3735928559];
  return { formatVersion: 1, seeds, scenarios: seeds.map(scenario) };
}
