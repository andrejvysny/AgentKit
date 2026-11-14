import { describe, expect, test } from "bun:test";
import { buildDataset } from "./fixtures";
import { scoreCapture, scoreRetrieval, search } from "./scoring";

const data = buildDataset();
describe("synthetic evaluation", () => {
  test("record identifiers do not disclose language or validity labels", () => {
    for (const record of data.records) {
      expect(record.id).not.toMatch(/^(en|sk|old|new|context)-/);
    }
  });
  test("covers required sizes and labels with valid provenance", () => {
    expect(data.records.length).toBe(500);
    expect(data.questions.length).toBeGreaterThanOrEqual(150);
    expect(data.turns.length).toBeGreaterThanOrEqual(100);
    expect(
      data.turns.every((turn) => /^turn-[a-f0-9]{16}$/.test(turn.id)),
    ).toBe(true);
    expect(data.turns.filter((turn) => turn.durable).length).toBe(
      data.turns.length / 2,
    );
    expect(new Set(data.questions.map((q) => q.subset)).size).toBe(7);
    const ids = new Set(data.records.map((record) => record.id));
    for (const question of data.questions) {
      for (const id of question.relevant) expect(ids.has(id)).toBe(true);
    }
    expect(data.poisoning.map((item) => item.source).sort()).toEqual([
      "child",
      "paste",
      "tool",
    ]);
  });
  test("inference export hides labels and fixture ordering", () => {
    const result = Bun.spawnSync(["bun", "scripts/evals/run.ts", "inputs"]);
    expect(result.exitCode).toBe(0);
    const inputs = JSON.parse(result.stdout.toString());
    expect(
      inputs.questions.every(
        (q: Record<string, unknown>) => !("relevant" in q) && !("subset" in q),
      ),
    ).toBe(true);
    expect(
      inputs.turns.every((t: Record<string, unknown>) => !("durable" in t)),
    ).toBe(true);
    expect(
      inputs.poisoning.every(
        (p: Record<string, unknown>) => !("expectedCapture" in p),
      ),
    ).toBe(true);
    expect(inputs.turns.map((t: { id: string }) => t.id)).toEqual(
      inputs.turns.map((t: { id: string }) => t.id).sort(),
    );
    expect(inputs.questions.map((q: { id: string }) => q.id)).toEqual(
      inputs.questions.map((q: { id: string }) => q.id).sort(),
    );
  });
  test("uses FTS5 with current and historical validity", () => {
    const current = data.questions.find((q) => q.subset === "supersession")!;
    const historical = data.questions.find((q) => q.subset === "as_of")!;
    expect(search(data.records, current)).toContain(current.relevant[0]);
    expect(search(data.records, current)).not.toContain(historical.relevant[0]);
    expect(search(data.records, historical)).toContain(historical.relevant[0]);
    expect(search(data.records, historical)).not.toContain(current.relevant[0]);
  });
  test("retrieval and capture metrics count errors and abstentions", () => {
    const questions = [
      { id: "a", subset: "direct", query: "x", relevant: ["r1"] },
      { id: "b", subset: "direct", query: "y", relevant: ["r2"] },
      { id: "c", subset: "no-answer", query: "z", relevant: [] },
    ];
    const report = scoreRetrieval(questions, {
      a: ["wrong", "r1"],
      b: [],
      c: ["wrong"],
    });
    expect(report.direct.recallAt5).toBe(0.5);
    const multiple = scoreRetrieval(
      [{ id: "m", subset: "direct", query: "x", relevant: ["r1", "r2"] }],
      { m: ["r1"] },
    );
    expect(multiple.direct.recallAt5).toBe(0.5);
    expect(() => scoreRetrieval(questions, {})).toThrow();
    expect(report.direct.mrr).toBe(0.25);
    expect(report["no-answer"].noAnswerAccuracy).toBe(0);
    expect(
      scoreCapture(
        [
          { id: "a", durable: true },
          { id: "b", durable: false },
        ],
        { a: true, b: true },
      ),
    ).toEqual({ tp: 1, fp: 1, fn: 0, precision: 0.5, recall: 1 });
    expect(() => scoreCapture([{ id: "a", durable: true }], {})).toThrow();
  });
});
