# Package T2.1: model profile

Requirements: TU-15, and the contract and validation parts of TU-16, TU-19,
TU-22. ADR 0030. Tests written, NOT run (validation deferred to the owner).

## What was built

- Contract `AiModelToolProfile` (Rust DTO, `schema_bundle.json`, fixtures,
  `packages/contracts/src/model-profile.ts`); `AiSessionPrefix` gained
  `modelProfileDigest`.
- `agentkit_core::model_profile`: built-in profiles, resolution, validation,
  digest, prefix stamping, `frozen_profile`.
- `agentkit_sessions`: `read_model_profile`, `with_model_profile`.
- `chat/prefix.rs`: stamps id, dialect version and digest into the prefix.

## Tests written (not run)

- `core/tests/model_profile.rs`: Qwen values against a golden fixture, all
  built-ins valid, generic conservative, provider and model matching, limit
  order and the empty-source skip, user profile precedence, estimated limits
  refused for a qualified model, allow-list rejection, malformed records,
  prefix stamping, digest, changed profile does not reach a running session.
- `contracts/tests/model_tool_profile.rs`: schema, round trip, refusals,
  closed body extras, prefix digest field.
- `chat/tests/session_model_profile.rs`: live session records the frozen
  profile; no refreeze on later turns; no profile means no fields.

## Not done

- Applying the profile to requests (T2.2), the live probe run (TU-22), and the
  per-model settings UI/storage (TU-19 host side).
