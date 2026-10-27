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

`packages/toolpacks-coding` adapts MIT-licensed code. Each adapted file starts with a header naming the upstream
repository, path and commit, plus the upstream copyright line. Nothing was copied wholesale; each file is a
selective, TypeScript-strict rewrite. Neither upstream repository was modified.

### Pi (`earendil-works/pi`, v1.4.0, commit `4a080252f06caf63a525d7873c5d94d3c23887e0`; Copyright (c) 2025 Mario Zechner; MIT)

| AgentKit file | Upstream path |
|---|---|
| `packages/toolpacks-coding/src/edit-engine.ts` | `packages/coding-agent/src/core/tools/edit-diff.ts`, `edit.ts` |
| `packages/toolpacks-coding/src/truncate.ts` | `packages/coding-agent/src/core/tools/truncate.ts` |
| `packages/toolpacks-coding/src/mutation-queue.ts` | `packages/coding-agent/src/core/tools/file-mutation-queue.ts` |
| `packages/toolpacks-coding/src/process.ts` | `packages/coding-agent/src/utils/shell.ts` (`killProcessTree`), `packages/coding-agent/src/core/tools/bash.ts` (timeout, tail truncation, full log) |

### OneMind (`andrejvysny/OneMind`; Copyright (c) 2026 OneMind contributors; MIT)

Commits: `paths.ts` and `search.ts` from `51abbbd` (the last change to those upstream files); `sandbox.ts` and `guard.ts` from `100dad7` (upstream files unchanged since).

| AgentKit file | Upstream path |
|---|---|
| `packages/toolpacks-coding/src/paths.ts` | `src-ts/src/code/exploration.ts` (lines 14-128: `resolveInsideRoot`, sensitive lists, `.env*` handling) |
| `packages/toolpacks-coding/src/search.ts` | `src-ts/src/code/pi-worker/search.ts` |
| `packages/toolpacks-coding/src/sandbox.ts` | `src-ts/src/code/pi-worker/shell-isolation.ts` (lines 5-90) |
| `packages/toolpacks-coding/src/guard.ts` (role ceiling table only) | `src-ts/src/code/tool-authority.ts`, `modules/code/shared/agent-settings.ts` (lines 23-27) |
