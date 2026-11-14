import { Database } from "bun:sqlite";
import type { Question, RecordItem } from "./fixtures";

type Metrics = {
  count: number;
  answerable: number;
  noAnswer: number;
  recallAt5: number | null;
  mrr: number | null;
  noAnswerAccuracy: number | null;
};

export function search(records: RecordItem[], question: Question): string[] {
  const db = new Database(":memory:");
  try {
    db.run(
      "CREATE VIRTUAL TABLE memory USING fts5(id UNINDEXED, text, validFrom UNINDEXED, validTo UNINDEXED)",
    );
    const insert = db.prepare("INSERT INTO memory VALUES (?, ?, ?, ?)");
    for (const record of records)
      insert.run(record.id, record.text, record.validFrom, record.validTo);
    const tokens = question.query.match(/[\p{L}\p{N}]+/gu) ?? [];
    if (tokens.length === 0) return [];
    const match = tokens.map((token) => `"${token}"`).join(" OR ");
    const date = question.asOf ?? "9999-12-31";
    return (
      db
        .query(
          "SELECT id FROM memory WHERE memory MATCH ? AND validFrom <= ? AND (validTo IS NULL OR validTo > ?) ORDER BY bm25(memory), id LIMIT 5",
        )
        .all(match, date, date) as { id: string }[]
    ).map((row) => row.id);
  } finally {
    db.close();
  }
}

export function scoreRetrieval(
  questions: Question[],
  predictions: Record<string, string[]>,
): Record<string, Metrics> {
  const report: Record<string, Metrics> = {};
  for (const subset of new Set(questions.map((question) => question.subset))) {
    const group = questions.filter((question) => question.subset === subset);
    for (const question of group)
      if (!Array.isArray(predictions[question.id]))
        throw new Error(`Missing retrieval prediction: ${question.id}`);
    const answerable = group.filter((question) => question.relevant.length > 0);
    const noAnswer = group.filter((question) => question.relevant.length === 0);
    const ranks = answerable.map((question) => {
      const rank = (predictions[question.id] ?? [])
        .slice(0, 5)
        .findIndex((id) => question.relevant.includes(id));
      return rank < 0 ? 0 : 1 / (rank + 1);
    });
    report[subset] = {
      count: group.length,
      answerable: answerable.length,
      noAnswer: noAnswer.length,
      recallAt5: answerable.length
        ? answerable.reduce((sum, question) => {
            const found = new Set(predictions[question.id].slice(0, 5));
            return (
              sum +
              question.relevant.filter((id) => found.has(id)).length /
                question.relevant.length
            );
          }, 0) / answerable.length
        : null,
      mrr: answerable.length
        ? ranks.reduce((sum, rank) => sum + rank, 0) / answerable.length
        : null,
      noAnswerAccuracy: noAnswer.length
        ? noAnswer.filter(
            (question) => (predictions[question.id] ?? []).length === 0,
          ).length / noAnswer.length
        : null,
    };
  }
  return report;
}

export function scoreCapture(
  turns: { id: string; durable: boolean }[],
  predictions: Record<string, boolean>,
) {
  let tp = 0,
    fp = 0,
    fn = 0;
  for (const turn of turns) {
    if (typeof predictions[turn.id] !== "boolean")
      throw new Error(`Missing boolean prediction: ${turn.id}`);
    if (predictions[turn.id] && turn.durable) tp++;
    if (predictions[turn.id] && !turn.durable) fp++;
    if (!predictions[turn.id] && turn.durable) fn++;
  }
  return {
    tp,
    fp,
    fn,
    precision: tp + fp ? tp / (tp + fp) : null,
    recall: tp + fn ? tp / (tp + fn) : null,
  };
}
