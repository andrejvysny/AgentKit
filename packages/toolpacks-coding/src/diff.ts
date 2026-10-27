import { splitLines } from "./truncate.js";

/** Cells of the LCS table above which the middle is shown as one block. */
const MAX_LCS_CELLS = 4_000_000;
const CONTEXT = 3;

type Op = { kind: " " | "-" | "+"; text: string };

function diffOps(a: string[], b: string[]): Op[] {
  let head = 0;
  while (head < a.length && head < b.length && a[head] === b[head]) head++;
  let tail = 0;
  while (
    tail < a.length - head &&
    tail < b.length - head &&
    a[a.length - 1 - tail] === b[b.length - 1 - tail]
  )
    tail++;
  const midA = a.slice(head, a.length - tail);
  const midB = b.slice(head, b.length - tail);
  const ops: Op[] = a.slice(0, head).map((text) => ({ kind: " ", text }));
  if (midA.length * midB.length > MAX_LCS_CELLS) {
    for (const text of midA) ops.push({ kind: "-", text });
    for (const text of midB) ops.push({ kind: "+", text });
  } else {
    ops.push(...lcsOps(midA, midB));
  }
  for (const text of a.slice(a.length - tail)) ops.push({ kind: " ", text });
  return ops;
}

function lcsOps(a: string[], b: string[]): Op[] {
  const width = b.length + 1;
  const table = new Uint32Array((a.length + 1) * width);
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      table[i * width + j] =
        a[i] === b[j]
          ? (table[(i + 1) * width + j + 1] as number) + 1
          : Math.max(
              table[(i + 1) * width + j] as number,
              table[i * width + j + 1] as number,
            );
    }
  }
  const ops: Op[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      ops.push({ kind: " ", text: a[i] as string });
      i++;
      j++;
    } else if (
      (table[(i + 1) * width + j] as number) >=
      (table[i * width + j + 1] as number)
    ) {
      ops.push({ kind: "-", text: a[i++] as string });
    } else {
      ops.push({ kind: "+", text: b[j++] as string });
    }
  }
  while (i < a.length) ops.push({ kind: "-", text: a[i++] as string });
  while (j < b.length) ops.push({ kind: "+", text: b[j++] as string });
  return ops;
}

export interface UnifiedDiff {
  diff: string;
  added: number;
  removed: number;
}

/** Minimal unified diff (3 lines of context) between two texts. */
export function unifiedDiff(
  path: string,
  before: string,
  after: string,
): UnifiedDiff {
  const ops = diffOps(splitLines(before), splitLines(after));
  const changed = ops
    .map((op, index) => (op.kind === " " ? -1 : index))
    .filter((index) => index >= 0);
  const added = ops.filter((op) => op.kind === "+").length;
  const removed = ops.filter((op) => op.kind === "-").length;
  if (changed.length === 0) return { diff: "", added, removed };
  const lines = [`--- a/${path}`, `+++ b/${path}`];
  // oldPos[i] / newPos[i]: 1-based line numbers of ops[i] in each file.
  const oldPos: number[] = [];
  const newPos: number[] = [];
  let o = 1;
  let n = 1;
  for (const op of ops) {
    oldPos.push(o);
    newPos.push(n);
    if (op.kind !== "+") o++;
    if (op.kind !== "-") n++;
  }
  let cursor = 0;
  while (cursor < changed.length) {
    let first = changed[cursor] as number;
    let last = first;
    cursor++;
    while (
      cursor < changed.length &&
      (changed[cursor] as number) - last <= CONTEXT * 2 + 1
    ) {
      last = changed[cursor] as number;
      cursor++;
    }
    first = Math.max(0, first - CONTEXT);
    last = Math.min(ops.length - 1, last + CONTEXT);
    const slice = ops.slice(first, last + 1);
    const oldCount = slice.filter((op) => op.kind !== "+").length;
    const newCount = slice.filter((op) => op.kind !== "-").length;
    const oldStart =
      oldCount === 0 ? (oldPos[first] as number) - 1 : oldPos[first];
    const newStart =
      newCount === 0 ? (newPos[first] as number) - 1 : newPos[first];
    lines.push(`@@ -${oldStart},${oldCount} +${newStart},${newCount} @@`);
    for (const op of slice) lines.push(`${op.kind}${op.text}`);
  }
  return { diff: lines.join("\n"), added, removed };
}
