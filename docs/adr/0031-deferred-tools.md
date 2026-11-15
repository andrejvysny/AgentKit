# ADR 0031: Deferred tools

Status: accepted (2026-11-15). Implements CE-08 and CE-09 of the context
efficiency spec (roadmap package 2.3). Builds on ADR 0028 (frozen tool
manifest, catalog) and ADR 0029 (failure type). Toolsets and the load modes
(`indirect`, `promote`) are native tool use T4.1 and sit on this design.

## Context

A session sent every tool definition on every turn. With pages, calendar,
terminals, artifacts and connected servers that is thousands of tokens that
the model rarely reads, and the array could not change without breaking the
prompt cache. Package 1.3 froze the array and gave late contributors a
deferred catalog, but nothing could call a catalog tool.

## Decision

1. **Direct and deferred.** A session's first turn splits the tools. Direct
   tools are sent every turn. Deferred tools are listed in the catalog
   (`AiSessionPrefix.deferredTools`) and are found, described and run through
   three meta tools: `tool_search`, `tool_describe`, `tool_call`.
2. **The classification is an input, not a profile type.**
   `agentkit_core::Classification` carries a `DirectSet` (exactly these are
   direct; the host takes it from the model profile's `direct_tools`), each
   tool's own `ToolLoading` hint (`Tool::loading`, forwarded by the governance
   wrappers) and the budget. `ChatExecutorOptions.tool_classification` is the
   switch: absent, every tool is sent as before.
3. **Small manifests stay direct.** A manifest within 3,000 estimated tokens
   and 24 definitions is all direct and the meta tools are left out of the
   array. Assumption: tune with the replay harness.
4. **Frozen with the manifest.** The classification is made once, on the first
   turn, and recorded as the manifest (direct tools plus the meta tools) and
   the catalog (the rest). Later turns follow the record. A late contributor
   joins the catalog and never the array. The meta tools are never "late".
5. **The array never changes.** `tool_call` is the indirect mode of spec 06:
   the tools array is fixed, calls nest one level deeper. Promotion of a
   loaded tool into the array (T4.1) is a later re-freeze and needs no change
   here.
6. **Governance parity.** The call pipeline replaces
   `tool_call {name, arguments}` by the inner call under the same call id
   before any guard runs (`core/src/meta_call.rs`). Resolve, normalise,
   validate, run hooks, the guards and approvals wrapped around the tool,
   receipts, events and the call record then name the inner tool and its
   arguments. `tool_call` decides nothing and has no authority; its effect is
   `read` because the contract has no `delegating` effect. Hosts that compare
   a declared call with an approval or a receipt (`resolve_approved_calls`,
   history reconciliation) unwrap with `unwrap_tool_call`, because both name
   the inner tool. A guard-withheld tool is refused through `tool_call`
   exactly as directly (`tool_not_allowed`).
7. **Refusals at the pipeline run nothing.** An unknown name is
   `tool_not_found` (with the closest names), a cataloged tool that nothing
   offers is `tool_unavailable`, arguments that do not fit the schema are
   `invalid_arguments` with the schema in the message (CE-09). None of them
   starts a guard or a body, and none writes a receipt. A direct call of a
   deferred tool is `tool_deferred`, which says how to call it.
8. **Discovery is not authorization.** A catalog entry says only
   `available: false` when no contributor offers the tool or a guard withheld
   it. A search result is no grant.
9. **S2 catalog index.** S2 ends with one line per deferred namespace
   (`- namespace (count): purpose`), sorted, built from the definitions. The
   text is stored in the prefix (`extra.deferredIndex`) at the first freeze
   and reused verbatim, so a late tool does not rewrite S2. The namespace is
   the part of the name before the last `__`, else the first segment of the
   capability, until toolsets exist.
10. **Search.** Lexical and deterministic: a query word scores once by its
    heaviest match, name or namespace 3, parameter name 2, description 1;
    ties break by name. Results are compact (name, namespace, short
    description, effect, parameter names with `*` for required); the schema is
    `tool_describe`'s job.

## Consequences

- One lint exception: `tool_call.arguments` is an open object because it
  carries another tool's own arguments. Every other meta tool parameter
  follows rules L1 to L12.
- In an all-direct session the meta tools are registered but not offered, so
  a late contributor's tool stays uncallable (`tool_unavailable`) there.
- Models call deferred tools less reliably. Keep frequent tools direct, and
  widen the set where the evaluation shows a drop.

## Not decided here

Toolsets, `toolset_load`, the promote mode and the recommender (T4.1); the
`tools.deferred_call` event (needs a contract change); wiring the model
profile's `direct_tools` and `max_visible_tools` into the classification.
