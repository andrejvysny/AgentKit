# Package 0.5: synthetic lexical baseline

Generated with `bun scripts/evals/run.ts retrieval` on Bun 1.4.0, then formatted using the repository-pinned Biome. Reproduction and byte comparison commands are in `scripts/evals/README.md`. Full metrics and per-question ranked IDs are in `retrieval-baseline.json`; fixture construction, commands, scoring definitions, and limitations are in `scripts/evals/README.md`.

| Subset | Count | Recall@5 | MRR | Empty-result accuracy |
| --- | ---: | ---: | ---: | ---: |
| direct | 40 | 1 | 0.9875 | — |
| paraphrase | 20 | 1 | 0.8933 | — |
| Slovak | 20 | 1 | 1 | — |
| cross-language | 20 | 0.9 | 0.3933 | — |
| supersession | 20 | 1 | 1 | — |
| as_of | 20 | 1 | 1 | — |
| no-answer | 20 | — | — | 0 |

FTS5 has no calibrated low-score threshold in this baseline: empty-result accuracy is a stricter observable abstention proxy, not the spec's low-score-result rate. All no-answer questions share indexed terms and return results. Temporal metadata is supplied as an oracle. Synthetic scenarios and combinatorial background records make these numbers unsuitable for model selection or production thresholds. No capture model or poisoning defense was run. Real owner-reviewed examples remain necessary to assess release gates.

Validation: initial `bun test scripts/evals/eval.test.ts` failed because `./fixtures` did not exist. A later regression test exposed incorrect multi-relevant recall (1 instead of 0.5). After implementation, 5 tests passed (664 assertions); strict TypeScript check passed with `npx tsc --noEmit --strict --target esnext --module preserve --moduleResolution bundler --types bun --skipLibCheck scripts/evals/{fixtures,scenarios,scoring,run,eval.test}.ts`. Owned TypeScript files were formatted with Biome.

## Scope and final qualification

The owner explicitly approved synthetic examples. The dataset has 500 records,
160 retrieval questions, 100 labelled owner turns and three source-poisoning
fixtures. This closes package 0.5 infrastructure, not MEM-13 model/capture gates.
Auto-capture remains off; no actual capture implementation is evaluated here.
Ground-truth labels are separated from inference inputs. Opaque record IDs were
added after the attached regression exposed label-correlated IDs. Constants were
moved out of long fixture functions to meet the code-size convention; complete
dataset JSON was byte-identical before and after that refactor.

The first full local run passed 17/18 checks. Four existing Claude process tests
failed with `frame within budget: Elapsed(())` under concurrent build/test load.
All 108 unchanged Claude tests passed in isolation. Repeating the complete Rust
workspace with `RUST_TEST_THREADS=1` passed 2,157 tests across 243 binaries; the
setting isolates independent test cases and changes no assertion, process deadline
or concurrency exercised within a test. Lint also passed again after the fixture
refactor. All 18 required checks therefore have passing final results, including
246 Client/React tests, 753 native/core tests, builds, architecture, API/contracts
and smoke. The original timeout remains documented; it was not fixed by changing
production code or weakening a guard.

Final targeted checks: five tests/664 assertions, strict npx tsc, byte-identical
formatted lexical baseline reproduction, and byte-for-byte source parity with
the isolated qualification copy. The architecture allowlist is unchanged. Logs
retain initial and corrected runner results and the substantive opaque-ID red.
No provider calls, models, real owner data or Plane writes were used.
