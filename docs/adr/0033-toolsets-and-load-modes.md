# ADR 0033: Toolsets and load modes

Status: accepted (2026-11-15). Implements TU-27 of the native tool use spec
(package T4.1). Builds on ADR 0031 (deferred tools), ADR 0030 (model
profile) and ADR 0032 (dialect contract).

## Context

ADR 0031 grouped deferred tools by a stand-in rule (the name before the last
`__`, else the first segment of the capability) and gave the model one line
per group in S2. The routing advice ("use `edit_file`, not the shell") sat in
the system prompt for every turn, whether or not the tools were in play.

## Decision

1. **A toolset is the unit a model loads.** `Toolset { id, title, card,
   guide, topics, tools }`. Ids: `files`, `shell`, `git`, `plan`,
   `code_intel`, `delegation`, `meta`; each connected server is a dynamic
   toolset named by the prefix of its tools (`mcp__alias`). The stand-in
   `namespace_of` stays only as that fallback.
2. **Membership is declared.** A toolset lists its tools by canonical id, or a
   tool names its toolset with `Tool::toolset()` (which wins; wrappers forward
   it). Owners of tools offer their toolsets: `ToolContributor::toolsets()`,
   registered with the staged registry. Canonical ids are the key, so a
   dialect that renames tools changes nothing here.
3. **Cards.** One line per toolset in the S2 catalog index, merged with the
   ADR 0031 line: `- files (7): <card>`. A toolset appears when it has a
   deferred member or a guide. The text depends only on definitions and cards,
   so it is cache-stable, and the session freezes it (`extra.deferredIndex`).
4. **Guides.** A versioned text file per toolset beside the crate that owns
   the tools (`guides/<toolset>.md`, `version:` front matter, included with
   `include_str!`), at most 600 tokens: which tool when, the usual order, two
   worked examples, common mistakes. Optional sub-guides by `topic`. Tool
   names in a guide are `{{tool:<canonical id>}}` placeholders; `toolset_load`
   renders them to the names the session's dialect offers (a split tool reads
   "a, b or c").
5. **`toolset_load(toolset, topic?)`** is the fourth meta tool. It returns the
   guide and the compact definitions of the toolset's tools as the tool
   result. A toolset none of whose tools the session has is not found. The
   meta tools are listed in `META_TOOLS`; `tool_call` cannot target them.
6. **Two load modes**, from the frozen profile's `load_mode`
   (`Classification::load_mode`, also `from_profile` for `direct_tools` and
   `max_visible_tools`):
   - `indirect`: the tools array never changes; loaded tools run through
     `tool_call`.
   - `promote`: `toolset_load` only records the request. At the next turn
     boundary (the top of the run's next model iteration, never mid-turn) the
     registry adds the toolset's deferred tools to the array and the host
     records it through `PromotionObserver`: the session manifest is
     re-stamped under the next prefix revision, the toolset leaves the
     deferred catalog, and one `run.warning` with code `toolset_promoted`
     (`toolset`, `tools`, `revision`) is emitted. A direct call to a tool in
     the same batch as its load is refused with `tool_deferred`. Cost is one
     prompt-cache rebuild per load, `t_rebuild ~ N_prompt / r_prefill`.
7. **Meta tools stay while a contributor can be late.**
   `ToolContributor::can_join_late()` (a connected-server contributor returns
   true) makes the session keep the meta tools in the array even when every
   tool is direct (`Classification::keep_meta`); without them a late tool is
   unreachable.
8. **Profile wiring.** When the host passes no classification, the chat
   executor builds one from the session's frozen model profile.
9. **Lint.** `tool_call.arguments` is an open object by design. Lint rule L4
   treats an object with no `properties` as opaque (same rule as OneMind
   T1.4); there is no exception list.

## Consequences

- S2 gets one line per toolset; routing rules move into guides.
- A promote session pays one cache rebuild per load and no nesting; an
  indirect session pays no rebuild and nests arguments one level.
- Guides are versioned text: a change is a version bump and shows in review.
- Old sessions (manifest without `toolset_load`) keep their array.

## Not decided

- Which mode a model uses is experiment X5 and probe P16.
- The recommender (ranks toolsets, writes one line into the turn context)
  is a later package.
