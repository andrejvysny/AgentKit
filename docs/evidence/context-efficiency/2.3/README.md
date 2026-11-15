# Package 2.3: deferred tools

Qualification: tests written, NOT run. Validation is deferred to the owner's
separate pass. Nothing in this note is a claim that a gate passed.

Requirements: CE-08 (meta tools, identical governance), CE-09 (argument
validation in `tool_call`). ADR: `docs/adr/0031-deferred-tools.md`.

## Built

- `core/src/deferred.rs`: `Classification`, `DirectSet`, `ToolLoading`,
  `Catalog` (lexical search), `catalog_index`.
- `core/src/meta_tools.rs`: `tool_search`, `tool_describe`, `tool_call`
  definitions and tools.
- `core/src/meta_call.rs`: `tool_call` unwrapped in the pipeline;
  `unwrap_tool_call`.
- `ToolRegistry`: `install_meta_tools`, `freeze_catalog`, `meta_active`,
  `catalog`, `deferred_hints`; `Tool::loading`.
- `ToolSurface::reconcile_with` classifies on the first turn; `index()`.
- `ChatExecutorOptions.tool_classification`; `chat/prefix.rs` binds the meta
  tools, builds and stores the S2 index, fills the catalog.
- Failure rows: `tool_not_found`, `tool_deferred`, `invalid_arguments`.

## Tests written (not run)

| File | Covers |
| --- | --- |
| `rust/crates/core/tests/deferred_tools.rs` | budget and direct-set classification, tool flag, index format and stability, search ranking and filters, describe, `tool_call` through the pipeline, normalisation, CE-09, tool_deferred, tool_not_found, late tool callable, withheld parity, meta schema rules, `unwrap_tool_call` |
| `rust/crates/chat/tests/deferred_tools.rs` | CE-08: real guards (allow, refuse, ask a human) give identical observations direct and through `tool_call`, approval names the inner tool and binds the inner arguments; live session: array, S2 and prefix stable across a late tool |

## Not covered or deferred

- Model profile wiring (`direct_tools`, `max_visible_tools`): the host passes a
  `DirectSet`.
- `tools.deferred_call` event; effect `delegating`; typed prefix field for the
  index (it is `extra.deferredIndex`).
- Approve-then-resume of a `tool_call` against the durable store: the unwrap is
  in `resolve_approved_calls` and `resolve_receipt`, covered by reading only.
- Public API snapshots (`rust/api/*.txt`) not regenerated.
