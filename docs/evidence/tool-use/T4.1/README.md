# Package T4.1: toolsets

Requirement: TU-27. ADR: `docs/adr/0033-toolsets-and-load-modes.md`.
Validation is deferred to the owner: these tests are written, NOT run.

## Tests written

- `rust/crates/core/tests/toolsets.rs`: guide front matter and placeholders;
  toolset membership (listed, declared, dynamic server, fallback); cards in the
  S2 index and their byte stability; `keep_meta`; profile to classification;
  `toolset_load` returns guide, topic guide, compact definitions and failures;
  indirect mode (tools array identical on every request, loaded tool run
  through `tool_call`); promote mode (array changes once at the turn boundary,
  same-batch direct call refused, one `toolset_promoted` event, observer
  called once, promoted twice only once).
- `rust/crates/runtime/tests/toolset_guides.rs`: the six AgentKit toolsets,
  every static tool in exactly one, cards one short line, guides versioned and
  under 600 tokens with two examples, placeholders name real tools and render
  to the common dialect's names.
- `rust/crates/runtime/tests/tool_schema_lint.rs`: the meta tools pass L1 to
  L12; an object with no `properties` is opaque for L4, an object with
  properties still is not.
- Updated: manifest name lists in `core/tests/deferred_tools.rs` and
  `chat/tests/deferred_tools.rs` now include `toolset_load`.

## Also in this package

- Governance wrappers (`GuardedTool`, `DirectWriteTool`, `ReceiptedTool`)
  forward `toolset()`, `arg_aliases()`, `constraints()` and
  `check_constraints()` to the inner tool (aliases were silently lost).

## Promoted tools in search and describe

`tool_search` / `tool_describe` read the catalog plus `Promotions`
(`meta_tools::with_promoted`): a promoted tool missing from the frozen catalog
is added, `tool_describe` reports `loading: "direct"` for promoted tools, and
`tool_search` flags them `in_tool_list`. Test:
`core/tests/toolsets.rs::promoted_tools_are_direct_for_describe_and_flagged_in_search`
(written, NOT run).
