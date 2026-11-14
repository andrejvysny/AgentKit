import { buildDataset } from "./fixtures";
import { scoreCapture, scoreRetrieval, search } from "./scoring";

const data = buildDataset();
const command = process.argv[2] ?? "retrieval";
if (command === "dataset") {
  console.log(JSON.stringify(data, null, 2));
} else if (command === "inputs") {
  console.log(
    JSON.stringify(
      {
        records: data.records,
        questions: data.questions
          .map(({ relevant, subset, ...input }) => input)
          .sort((a, b) => a.id.localeCompare(b.id)),
        turns: data.turns
          .map(({ durable, ...input }) => input)
          .sort((a, b) => a.id.localeCompare(b.id)),
        poisoning: data.poisoning.map(({ expectedCapture, ...input }) => input),
      },
      null,
      2,
    ),
  );
} else if (command === "retrieval") {
  const predictions = Object.fromEntries(
    data.questions.map((question) => [
      question.id,
      search(data.records, question),
    ]),
  );
  console.log(
    JSON.stringify(
      {
        provenance: data.provenance,
        method:
          "SQLite FTS5 unicode61, OR tokens, bm25, top 5; validity filter before ranking",
        metrics: scoreRetrieval(data.questions, predictions),
        predictions,
      },
      null,
      2,
    ),
  );
} else if (command === "capture" || command === "poisoning") {
  const path = process.argv[3];
  if (!path)
    throw new Error(
      `Usage: bun scripts/evals/run.ts ${command} predictions.json`,
    );
  const predictions = (await Bun.file(path).json()) as Record<string, boolean>;
  const labels =
    command === "capture"
      ? data.turns
      : data.poisoning.map((item) => ({
          id: item.id,
          durable: item.expectedCapture,
        }));
  console.log(JSON.stringify(scoreCapture(labels, predictions), null, 2));
} else {
  throw new Error(`Unknown command: ${command}`);
}
