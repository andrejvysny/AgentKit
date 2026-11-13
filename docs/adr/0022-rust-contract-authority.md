# ADR 0022: Rust is the wire-contract source of truth

Status: accepted (2026-11-09). Supersedes "TypeBox is the single source of
truth" in `docs/contracts.md` and the TypeBox-to-Rust codegen described in
`docs/architecture.md` before this date. Follows ADR 0023 (TypeScript backend
retired).

## Context

`packages/contracts` (TypeBox) was the wire authority. `generate.mjs` (Bun)
turned it into 131 generated Rust files, `routes.rs` and `schema_bundle.json`.
With the TypeScript backend gone, every producer and validator of the wire is
Rust; TypeScript only describes the shapes for clients. Generating the
authority's code from its consumer inverted the dependency.

The generator also had a bug: it deduplicated structurally identical schemas
and kept the first name, so 94 files typed unrelated string fields as
`AiProviderKind` (for example `ChatDto.id`).

## Decision

1. The `agentkit-contracts` DTOs (`src/dto/*.rs`), the route table
   (`src/routes.rs`) and `src/schema_bundle.json` are hand-owned source,
   edited together in one change. No generator writes them.
2. The JSON Schema bundle stays a checked-in Rust-owned artifact; it is not
   derived with `schemars`. A trial showed `schemars` 1.0.4 cannot reproduce it:
   it emits draft 2020-12 `$schema`/`title`, `type: ["string","null"]` and
   `additionalProperties: true` where the bundle has draft-07 `anyOf`
   nullables and `patternProperties`, and the Rust DTOs carry none of the
   constraints the bundle states (512 `description`, 48 `minimum`, 33
   `maximum`, 22 `pattern`, 6 `minLength`, 6 `minItems`, 9 `format`).
   Rust tests (`tests/parity.rs`, `dto_roundtrips.rs`, goldens, conformance
   corpus) check the DTOs, routes and fixtures against the bundle.
3. `packages/contracts` stays hand-written TypeBox with an unchanged public API
   (`bun run check:api`). Generating TypeBox source from the bundle was
   rejected for now: it cannot keep the `Static<>`-derived type names, the
   documented hybrids (`AiToolResult<T>`, `AiRunEvent`, ...) and the frozen API
   report byte-identical. Authority is enforced instead by
   `scripts/tests/rust-contract-authority.test.ts`: every TypeBox schema must
   emit JSON equal to its bundle entry, and the name sets must match.
4. `scripts/export-rust-fixtures.ts` now reads schemas and run-event variants
   from the Rust bundle. Routes (pure data from `REST_ROUTES`) and golden
   traces stay exported from TypeScript and are checked against Rust by
   `tests/parity.rs`.
5. The dedupe naming bug is fixed in the DTO sources: plain-string fields are
   `String`; `AiProviderKind` remains only on the provider `kind` fields. The
   wire schema is unchanged; `rust/api/agentkit-contracts.txt` changes only in
   how those field types are spelled.

## Changing the wire contract

```sh
# 1. Edit rust/crates/contracts/src/dto/*.rs, routes.rs and schema_bundle.json together.
# 2. Mirror the change in packages/contracts/src/*.ts (TypeBox).
bun run contracts:generate     # re-export fixtures (rust/fixtures + crate copy)
bun run contracts:check        # fixtures current + TypeBox == Rust bundle
cd rust && cargo test --locked -p agentkit-contracts --all-features
node rust/scripts/public-api.mjs --update   # only if Rust API spelling changed
bun run check:api                           # TS API must stay as intended
```

## Consequences

- A contract change starts in Rust; TypeScript drift fails
  `contracts:check`.
- The bundle is edited by hand (one compact JSON line, 130 schemas). A
  follow-up should add a Rust generator or pretty-printed per-schema files so
  reviews are readable; that must be byte-identical or a separate change.
- Unrelated string fields no longer share a misleading type name in Rust.
- TypeBox remains a second hand-maintained copy until TypeScript generation
  becomes feasible.
