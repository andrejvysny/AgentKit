# AgentKit

AgentKit is an embeddable AI-assistant framework extracted from OpenPCB's
assistant module. This repo currently contains the loop runtime
(`packages/core`, published as `@agentkit/core`); contracts, host, and
testing packages are arriving in subsequent commits on this branch.

**Status:** pre-release, 0.x. APIs may change without notice.

> **Not published to npm.** The `@agentkit` npm scope is not yet
> verified/owned — do not publish any package under this scope until scope
> ownership is confirmed.

## Packages

- [`packages/core`](packages/core) — `@agentkit/core`: headless AI agent
  runtime primitives (providers, tools, prompts, run events). Pure
  TypeScript, single runtime dependency (`ajv`).

## Development

This is a Bun workspaces monorepo.

```sh
bun install
bun run typecheck
bun test
bun run build
```

## License

MIT — see [LICENSE](LICENSE). See [PROVENANCE.md](PROVENANCE.md) for the
history of how this code was extracted and relicensed.
