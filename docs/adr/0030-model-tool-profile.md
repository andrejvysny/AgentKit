# ADR 0030: Model tool profile

Status: accepted (2026-11-15). Implements TU-15 and the contract half of
TU-16, TU-19 and TU-22 of the native tool use spec (package T2.1). Builds on
ADR 0022 (Rust owns the wire contract) and ADR 0028 (frozen session prefix).

## Context

The only per-model switch was the rich-visuals gate and the only request
setting was temperature. Models differ in what they need: Qwen 3.8 27B wants
its own sampling sets, an effort word as a template option, tool choice `auto`
only, one system message first, and escaped tool-result tags. Without a
record of that, every request builder guesses, and a settings edit could
change a session in flight.

## Decision

1. **One Rust-owned record.** `AiModelToolProfile` (contracts crate, schema
   `AiModelToolProfileSchema`, TypeBox mirror in `model-profile.ts`) holds
   family, dialect id and version, direct tools, max visible tools, load mode,
   tool-choice policy, both sampling sets, reasoning (effort channel, effort
   map, send-back), body extras, message rules, limits, recovery and the
   toolsets the conformance matrix qualified.
2. **Built-ins and matching.** `anthropic`, `openai`, `generic`, `qwen3.8-27b`
   live in `agentkit_core::model_profile`. A model matches by provider and
   model-id pattern (`*` globs, case-blind), first rule wins: any provider with
   `*qwen3.8*27b*`; provider `anthropic*`; provider `openai`. Everything else,
   including OpenAI-compatible and OpenRouter ids of hosted models, gets
   `generic`, because their wire is not the first-party one.
3. **Limits come from, in order:** the user's setting, the proxy's model
   information, the server's model list, the probe, the profile default. A
   user-configured profile record is itself the user setting. A source without
   a context window is skipped. Output is never above the window.
4. **`estimated` is for unqualified models only.** A profile with
   `qualifiedToolsets` and limit source `estimated` fails validation.
5. **The body-extras allow-list is closed** (`chat_template_kwargs`,
   `parallel_tool_calls`). It is enforced three times: the schema
   (`additionalProperties: false`), the typed decoder, and
   `validate_profile_json`, which names the offending field first. Sampling
   values travel in `sampling`, not as extras.
6. **Frozen with the session.** The record is written once into chat metadata
   (`agentkit.session.modelProfile`, `with_model_profile` through the creation
   metadata). `frozen_profile` returns the stored record and never consults
   the resolver again. The prefix gets `modelProfileId`, `dialectVersion` and
   `modelProfileDigest` (SHA-256 of the canonical record), stamped by
   `chat/prefix.rs`; a re-freeze carries them. A changed built-in or setting
   therefore never reaches a running session.
7. **Not applied yet.** Request building (T2.2) reads the frozen record; this
   package only resolves, validates, freezes and records it.

## Consequences

- The Qwen limits (131,072 / 32,768) carry source `user`: they are the
  owner's served values from the spec, not an estimate, and stay valid when the
  profile becomes qualified. Any looked-up source replaces them.
- Direct-tool lists and the hosted visible-tool bound (24) are assumptions
  until experiment X5.
- The template option name that carries the effort word is not fixed here; the
  probe (P-checks, G3) and T2.2 decide it. The profile only says the channel is
  `template_option`.
- Adding an allow-listed field is a contract change (schema, Rust, TypeBox).

## Qualification (R21)

`qualifiedToolsets` is filled only from a qualification record
(`agentkit.tool-use-qualification/1`) produced by the production driver
(`rust/crates/runtime/tests/tool_use_qualification.rs`): real tool
definitions, the frozen profile and dialect, and the durable run loop, with
only external effects mocked behind their ports. A record is valid only for
the exact tuple of model, provider, server, profile id/version/digest, tool
schema digest, prompt digest and toolsets it measured. The older
`conformance --live` runner uses substitute tools and is smoke only. See
`docs/qualification/tool-use-qualification.md`.
