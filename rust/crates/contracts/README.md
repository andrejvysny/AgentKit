# AgentKit contracts for Rust

This crate is the wire source of truth (ADR 0022). It exposes the wire DTOs, all 20
run-event variants, and the 63 REST v1 operations. DTOs live in `src/dto/`, routes
in `src/routes.rs` and the embedded JSON Schema bundle in `src/schema_bundle.json`;
all three are hand-owned and edited together. `@agentkit/contracts` (TypeBox) mirrors
them and is checked against the bundle.

After a contract change, from the repository root:

```sh
bun run contracts:generate   # re-export fixtures from the bundle
bun run contracts:check      # fixtures current + TypeBox == bundle
cargo test --manifest-path rust/Cargo.toml -p agentkit-contracts --all-features
```

## Current export fixtures and migration delta

`rust/fixtures/schemas.json` and its packaged copy in `tests/fixtures` describe
the Rust bundle (routes and goldens come from TypeScript data). They are regenerated
together with their manifests through the exporter above. The Rust parity test compares the
entire embedded schema bundle with these exports, without migration exceptions;
exporter checks and release projection also reject any drift. Historical contract
snapshots remain available through Git rather than replacing this current check.
The manifest's `baselineSha` identifies the exporter lineage; its current payload
hashes identify the checked exports.

Against `f64c21e355e6f86a1d60024b45c2906cdade9758`, the schema delta is exactly
one optional `reasoningEffort` property on each of `SubmitMessageRequestSchema`
and `RegenerateMessageRequestSchema`. Each accepts only the string literals
`minimal`, `low`, `medium`, and `high`; omission remains valid, while null and
other values are rejected. Two event variants are added:
`run.tool.preparing` (`AiRunToolPreparingEventSchema`), informational progress
while a provider streams a tool call's arguments, and `run.progress`
(`AiRunProgressEventSchema`), the phase a streaming provider is busy with. The
teammate additions (persistent agents, teammate admission and mailbox, run
profiles, and the teammate HTTP routes and schemas) extend the schema and route
inventories. No required fields, existing properties, or golden traces change.
The inventories are 130 schemas, 63 routes, and 20 event variants. The
optionality and closed value set have a dedicated Rust parity regression for
both turn routes.

## Deserialization and validation

`deserialize_validated::<T>(schema_name, value)` applies the embedded Draft-07
schema before deserializing. `validate_schema` validates a wire value, and
`validate_request` selects the body schema for a `RestOperation`.

Plain Serde deserialization checks structural Rust types, discriminators,
required fields, and closed objects. Constraints such as URL patterns, numeric
bounds, and minimum array lengths require the validation helpers. Call those
helpers at untrusted input boundaries.

DTOs implement `JsonDeserialize` and custom Serde decoding that retain
arbitrary objects, including keys matching Serde's private number/raw-value
markers. This works through both `serde_json::from_str` and `from_value` for the
DTO structs and unions. Use `deserialize_validated` or
`JsonDeserialize::from_json` for container aliases such as `AiJsonSchemaObject`;
plain Serde decoding of an external `Value` or standard map still has Serde's
private-marker ambiguity.

Application structs can preserve JSON fields with
`#[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]`.
The adapter supports `Value`, `Vec`, `Option`, `BTreeMap`, `HashMap`, Serde maps,
and DTOs. Retain `#[serde(default)]` where omission is allowed. Serde
flattening and untagged-enum buffering discard the raw representation; implement
`JsonDeserialize` with explicit field extraction for those structures, as the
generator does.

`Optional<T>::Absent` preserves omission. `Optional<T>::Present(value)` preserves
a present field, including `Value::Null` when that schema permits arbitrary JSON.
`Nullable<T>` represents an explicitly nullable value; a required nullable field
still cannot be omitted. Object fields named `extra` retain unknown members
where the TypeScript schema permits them. Closed continuation objects reject
unknown fields.

JSON Schema `number` and `integer` values use `f64` to match JavaScript's numeric
range. Schema validation enforces integer constraints. `Clock` and `IdGenerator`
are injectable ports with no system-time or randomness implementation in the
contract layer.

## Schema resolution and idempotency

`compile_schema` always selects Draft-07 and installs a retriever that rejects
external references. Network and filesystem reference retrieval are also
disabled in dependency features. Local fragment references remain supported.
Built-in validators are compiled once and cached.

Pattern checks use an ECMAScript regular-expression engine. Property-pattern
and additional-property checks share the same matching rules, including Unicode
line terminators. Child schemas compile eagerly against the original root
resource, so local references and recursive schemas retain their resolution
context without compiling during validation. Weak callback references avoid a
cycle between the root context and its cached child validators.

`compile_tool_schema` applies the repository's distinct Ajv 8 + ajv-formats tool
profile. Its custom numeric and format checks preserve observed JavaScript
behavior, including floating-point `multipleOf`, enabled formats and ignored
unknown formats. Like the TypeScript tool registry, this profile strips `$id`
and `$schema` recursively before compilation; `compile_schema` retains reference
resource IDs. The format-pattern snapshot comes from installed ajv-formats
3.0.1. The shared `rust/fixtures/schema-validation.json` corpus is checked against
the actual TypeScript registry and Rust validator; it does not claim universal
equivalence between the two schema engines.

Non-strict tool schemas can themselves contain overflowing numeric constraints.
A compilation mirror preserves meta-schema checks while custom validators retain
the original infinite bounds/divisors. Length/item/property bounds use JavaScript
number comparisons, including limits beyond native integer ranges.

Native UTF-8 strings reject JSON escapes containing unpaired UTF-16 surrogates.
This two-case compatibility exception was explicitly approved on 2026-10-22;
valid surrogate pairs are preserved.

`parse_javascript_json` preserves JavaScript binary64 rounding, signed zero and
numeric overflow. Overflow is represented as a numeric Serde node (`1e400` or
`-1e400`); `javascript_number` exposes the corresponding infinity. Tool callbacks
receive that numeric node unchanged. Strict wire-number validation rejects
nonfinite numbers, while the tool profile matches Ajv's non-strict numeric rules.
`javascript_json_value` and `stringify_javascript_value` apply JSON.stringify
semantics at output/persistence boundaries, including nonfinite-to-null conversion,
JavaScript numeric rendering and integer-index property ordering. Plain
`Value::as_f64` cannot represent overflow; use `javascript_number` when interpreting
an arbitrary tool number. The shared corpus tests overflow constraints and
composition without suppressing those cases.

## Validation

```sh
cargo test --manifest-path rust/Cargo.toml -p agentkit-contracts --locked
cargo clippy --manifest-path rust/Cargo.toml -p agentkit-contracts --all-targets --locked -- -D warnings
cargo check --manifest-path rust/Cargo.toml -p agentkit-contracts --target wasm32-unknown-unknown --locked
```

Tests cover the frozen schema and route exports, all DTOs, all event
variants, every exported golden trace, missing versus null fields, unknown-field
round trips, closed discriminators, media/URL constraints, continuation bounds,
remote-reference refusal, local references, and actual TypeScript idempotency
vectors. The contract crate compiles for WebAssembly; native runtime services
live in other crates.
