# Synthetic memory evaluations

These deterministic fixtures contain no real owner examples. Run from the AgentKit root with Bun; no network, model, or existing database is required.

- `bun scripts/evals/run.ts dataset` emits labelled records, questions, owner turns and poisoning cases as JSON. Keep this ground truth away from systems being evaluated.
- `bun scripts/evals/run.ts inputs` emits the same inputs without `relevant`, `subset`, `durable`, or `expectedCapture` labels. Question, record, and turn IDs are opaque; questions and turns are sorted by ID to break label-correlated fixture order. Use these inputs for inference, then save predictions keyed by ID as a JSON object.
- `bun scripts/evals/run.ts retrieval` emits per-subset metrics and every top-five prediction for the built-in baseline. It builds a temporary SQLite FTS5 index using unicode61, ORs quoted query tokens, filters validity before BM25 ranking, and breaks ties by ID. Default date is after all current records; `asOf` uses `[validFrom, validTo)`.
- `bun scripts/evals/run.ts capture predictions.json` scores 100 owner turns. Each ID requires a boolean indicating whether the system would capture it.
- `bun scripts/evals/run.ts poisoning predictions.json` scores three separately labelled tool-output, pasted-text and child-agent cases using the same boolean format. False is expected for all three. These are source-boundary fixtures only; predictions from a real capture system are necessary to test it.
- `bun test scripts/evals/eval.test.ts` runs conformance tests. `scoreRetrieval` and `scoreCapture` are exported for other systems; both require predictions for every item.

To reproduce and compare the committed lexical baseline, format the raw output with the repository-pinned Biome before comparing bytes:

```sh
bun scripts/evals/run.ts retrieval > /tmp/agentkit-eval-raw.json
./node_modules/.bin/biome format --stdin-file-path=docs/evidence/context-efficiency/0.5/retrieval-baseline.json < /tmp/agentkit-eval-raw.json > /tmp/agentkit-eval-formatted.json
cmp /tmp/agentkit-eval-formatted.json docs/evidence/context-efficiency/0.5/retrieval-baseline.json
```

Recall@5 is the mean fraction of each answerable question's labelled relevant IDs retrieved in its first five results. MRR is the mean reciprocal rank of the first relevant ID within those five. No-answer accuracy is the fraction of unanswerable questions with an empty result. Null means the metric's denominator is zero. Capture precision and recall use turn-level boolean predictions; undefined denominators are null. The lexical baseline does not perform a relevance threshold or model-generated abstention, so any FTS match is returned even if irrelevant.

Limitations: records include 40 individually written facts, 20 dated updates with two versions each, and 420 combinatorial person-and-habit records. Owner turns combine 50 distinct durable statements with 50 templated task requests. Labels are synthetic, not owner-verified; cross-language questions share names or project identifiers with English-only target facts. No-answer cases deliberately overlap indexed terms. Temporal validity is given to the baseline rather than inferred. These fixtures exercise the harness and cannot establish real-world retrieval, capture, poisoning resistance, or model quality. Obtain diverse owner-reviewed examples before using results for a release gate. No capture system is evaluated here and no defaults are changed.
