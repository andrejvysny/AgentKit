# Developing / consuming `agentkit`

AgentKit ships as **one** installable package, `agentkit`, with subpath
imports (`agentkit/host`, `agentkit/adapters-sqlite`, …) backed by an
umbrella package (`packages/agentkit`) whose `dist/` is assembled from the
ten `@agentkit/*` source packages by `scripts/build-umbrella.mjs`. See
[`packages/agentkit/README.md`](packages/agentkit/README.md) for the
subpath table.

Three workflows are supported, picked based on how much you need to
iterate:

| Workflow                     | When to use                                             | Setup cost                        | Speed of feedback              |
| ----------------------------- | -------------------------------------------------------- | ---------------------------------- | -------------------------------- |
| **GitHub-tag install** (default) | You only consume `agentkit`; you don't edit this repo | none                                | seconds per release             |
| **`npm link`**                  | You're actively editing AgentKit and want a consumer to pick up changes | one-time `npm link` ritual          | rebuild + relink → next import  |
| **TypeScript path overlay**      | You're editing both repos at once and want IDE jump-to-source without a dist rebuild | drop-in `tsconfig.dev.json`        | instant (no build at all)        |

## 1. GitHub-tag install (the default)

Consumers add a dependency like:

```jsonc
"dependencies": {
  "agentkit": "github:andrejvysny/AgentKit#v0.4.0"
}
```

`npm install` or `bun install` clones the tagged **release branch** —
whose repo root already contains `packages/agentkit`'s committed `dist/`
and `package.json` (see "Releasing a new version" below) — so there is
**no `prepare` step and nothing to build**. Works under both npm and Bun.

## 2. `npm link` (recommended for active development)

Use this when you're editing a `packages/<pkg>/src` file in this repo and
want the change reflected in a consumer without bumping a version and
re-tagging.

### One-time, from this repo's root:

```bash
bun install
bun run build            # every source package's dist/
bun run build:umbrella   # assembles packages/agentkit/dist from the above
cd packages/agentkit
npm link
```

### From the consumer:

```bash
npm link agentkit
```

After editing a package's `src/`, rebuild before the consumer sees it —
there is no watch mode across the umbrella assembly step:

```bash
bun run build && bun run build:umbrella
```

### Caveats

- `npm link` symlinks point at `packages/agentkit/dist`, which is a
  **generated copy**, not a live view of the source packages — you must
  re-run `bun run build && bun run build:umbrella` after every edit.
- Some bundlers (Vite, esbuild) cache resolved paths; restart the
  consumer's dev server after linking/unlinking.

## 3. TypeScript path overlay (fastest, IDE-friendly)

For an even tighter loop — no `dist/` rebuild needed, even for types — drop
a `tsconfig.dev.json` into your consumer that maps `agentkit/*` directly to
each source package's `src/index.ts`:

```jsonc
{
  "extends": "./tsconfig.json",
  "compilerOptions": {
    "paths": {
      "agentkit/contracts": ["../AgentKit/packages/contracts/src/index.ts"],
      "agentkit/client": ["../AgentKit/packages/client/src/index.ts"],
      "agentkit/core": ["../AgentKit/packages/core/src/index.ts"],
      "agentkit/host": ["../AgentKit/packages/host/src/index.ts"],
      "agentkit/testing": ["../AgentKit/packages/testing/src/index.ts"],
      "agentkit/mcp-client": ["../AgentKit/packages/mcp-client/src/index.ts"],
      "agentkit/transport-http": [
        "../AgentKit/packages/transport-http/src/index.ts"
      ],
      "agentkit/adapters-memory": [
        "../AgentKit/packages/adapters-memory/src/index.ts"
      ],
      "agentkit/adapters-sqlite": [
        "../AgentKit/packages/adapters-sqlite/src/index.ts"
      ],
      "agentkit/runner-local": [
        "../AgentKit/packages/runner-local/src/index.ts"
      ]
    }
  }
}
```

Adjust the relative path if the two repos aren't checked out as siblings.
This affects **type resolution only** — runtime resolution still goes
through `node_modules`, so pair it with `npm link` (workflow 2) if you want
the runtime behavior to follow too. Not committed by default — opt in per
developer.

## Releasing a new version

`packages/agentkit/package.json`'s `version` is the **lockstep release
version** for the whole `agentkit` package — the individual
`@agentkit/*` packages under `packages/*` keep their own `-dev` versions
and are never tagged or released independently.

1. Bump `packages/agentkit/package.json`'s `version` to the release
   version (e.g. `0.4.0` — no `-dev` suffix).
2. Commit and push to the default branch.
3. Tag the commit `vX.Y.Z` (matching the version from step 1 exactly) and
   push the tag.
4. `.github/workflows/release.yml` does the rest: typecheck, test, build
   every source package, run `build:umbrella`, run `smoke:umbrella`
   (npm **and** bun), commit the built `packages/agentkit/dist`,
   subtree-split `packages/agentkit` into a `release/vX.Y.Z` branch, and
   re-point the `vX.Y.Z` tag at that split commit.

Consumers bump their pinned tag and run `npm install` / `bun install`.

**Warning:** the release branch and the tag it produces are **force-moved**
by CI every time the workflow runs for that tag. Never branch off
`release/vX.Y.Z` or build tooling that assumes it is an append-only ref —
treat it purely as an installable snapshot, not a development branch.
