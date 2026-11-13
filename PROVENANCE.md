# Provenance

`packages/core` was extracted from
[github.com/OpenPCB-app/shared](https://github.com/OpenPCB-app/shared) @
commit `1410e80db17ecd5a225afa0e1948461f0cfb7b79` (path `packages/ai-core`,
published as `@openpcb/ai-core` v0.4.0).

History was preserved via `git subtree split` (split tip
`de643c79934f64a6dfa4c022cbac1c7067bc4347`).

All 6 commits touching the package are sole-authored by
Andrej Vyšný \<vysnyandrej@gmail.com\>.

The package was previously licensed AGPL-3.0-or-later. It was relicensed to
MIT by the sole author and copyright holder on 2026-08-18.

The only runtime dependency (`ajv`) is consumed via npm; no third-party code
is vendored.

## Third-party adaptations

`packages/toolpacks-coding` (deleted 2026-11-02) and `packages/host/src/context/compactor.ts` (deleted 2026-11-09, ADR 0023; recoverable at commit `a31dcb1`) adapt
MIT-licensed code. Each adapted file starts with a header naming the upstream
repository, path and commit, plus the upstream copyright line. Nothing was copied wholesale; each file is a
selective, TypeScript-strict rewrite. Neither upstream repository was modified.

`rust/crates/toolpacks-coding` is a Rust port of `packages/toolpacks-coding` and now carries these
adaptations on its own: its `edit_engine.rs`/`truncate.rs`/`mutation_queue.rs`/`process.rs` descend from
the Pi-derived files, and `paths.rs`/`search.rs`/`sandbox.rs`/`guard.rs` (role-ceiling table) from the
OneMind-derived files listed
below. No upstream was vendored; each Rust module rewrites its TypeScript source, which carried the
upstream headers. The tables below list the Rust port paths for the deleted TypeScript originals; the
upstream mappings are unchanged.

### Pi (`earendil-works/pi`, v1.4.0, commit `4a080252f06caf63a525d7873c5d94d3c23887e0`; Copyright (c) 2025 Mario Zechner; MIT)

| AgentKit file | Upstream path |
|---|---|
| `rust/crates/toolpacks-coding/src/edit_engine.rs` | `packages/coding-agent/src/core/tools/edit-diff.ts`, `edit.ts` |
| `rust/crates/toolpacks-coding/src/truncate.rs` | `packages/coding-agent/src/core/tools/truncate.ts` |
| `rust/crates/toolpacks-coding/src/mutation_queue.rs` | `packages/coding-agent/src/core/tools/file-mutation-queue.ts` |
| `rust/crates/toolpacks-coding/src/process.rs` | `packages/coding-agent/src/utils/shell.ts` (`killProcessTree`), `packages/coding-agent/src/core/tools/bash.ts` (timeout, tail truncation, full log) |
| `packages/host/src/context/compactor.ts` (summarization and update prompts, cut-point rule) | `packages/coding-agent/src/core/compaction/compaction.ts` (`SUMMARIZATION_PROMPT`, `UPDATE_SUMMARIZATION_PROMPT`, `findCutPoint`) |
| `packages/host/src/context/compactor.ts` (system prompt, conversation serialization, file-operation tracking) | `packages/coding-agent/src/core/compaction/utils.ts` (`SUMMARIZATION_SYSTEM_PROMPT`, `serializeConversation`, `extractFileOpsFromMessage`, `formatFileOperations`) |
| `packages/core/src/context/token-estimator.ts` (chars/4 heuristic; constants only) | `packages/coding-agent/src/core/compaction/compaction.ts` (`estimateTokens`) |

### OneMind (`andrejvysny/OneMind`; Copyright (c) 2026 OneMind contributors; MIT)

Commits: `paths.ts` and `search.ts` from `51abbbd` (the last change to those upstream files); `sandbox.ts` and `guard.ts` from `100dad7` (upstream files unchanged since).

| AgentKit file | Upstream path |
|---|---|
| `rust/crates/toolpacks-coding/src/paths.rs` | `src-ts/src/code/exploration.ts` (lines 14-128: `resolveInsideRoot`, sensitive lists, `.env*` handling) |
| `rust/crates/toolpacks-coding/src/search.rs` | `src-ts/src/code/pi-worker/search.ts` |
| `rust/crates/toolpacks-coding/src/sandbox.rs` | `src-ts/src/code/pi-worker/shell-isolation.ts` (lines 5-90) |
| `rust/crates/toolpacks-coding/src/guard.rs` (role ceiling table only) | `src-ts/src/code/tool-authority.ts`, `modules/code/shared/agent-settings.ts` (lines 23-27) |
