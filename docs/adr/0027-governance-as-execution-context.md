# ADR 0027: Governance as an execution context

Status: accepted (2026-11-12). Extends ADR 0021 (layers) and prepares
AGENTKIT-141 (native journal).

## Context

ADR 0021 put `agentkit-governance` in layer 2 as ports only (308 lines). The
implementation — staging, durable execution receipts, write policy, the
proposal service and proposal tool, native command drain — grew inside
`agentkit-chat` (`chat/src/governance/*`, `chat/src/proposals.rs`, ~3.2k
lines plus tests). It uses almost nothing else from chat.

ADR 0026 added a second execution owner: a vendor agent runtime driven by
`agentkit-native-exec`. Its broker needs the same receipts, staging and
approvals the model loop uses, and ADR 0026 forbids it to depend on chat to
get them. With the implementation inside chat, the only way for the broker to
reach durability was through the model loop's crate — which also kept
`GovernanceError::Execution(String)` as the catch-all the R7 rule rejects.

## Decision

`agentkit-governance` becomes a layer-3 application context. The chain is
`engine ← sessions ← governance ← chat ← agents`, enforced by
`CONTEXT_DEPS` in `check-arch`:

- governance depends on engine (and layer ≤ 2); never on sessions or chat.
- chat, sessions and agents may depend on governance.
- The moved code still names `SqliteStore` directly; that edge is allowlisted
  (`R1b agentkit-governance -> agentkit-store`) and tagged for removal by
  AGENTKIT-131 (repository ports), per the R13 rule.

Moved as pure renames (no behaviour change): `governance/{core_run_ownership,
durable_execution, execution_completion, native_command_drain, policy,
proposal_support, proposal_tool, staging}.rs` and `proposals.rs` into the
governance crate; `Authorization`, `MutationIdentity` and `ProposalApplier`
moved out of chat's ports (chat re-exports them for source compatibility).
Integration tests stay in `chat/tests` — they exercise behaviour through the
chat executor and use its fixtures.

Errors: every failure the crate itself produces is a typed
`GovernanceError` variant (18), with `Core`/`Store` carriers. The former
`Execution(String)` catch-all is renamed `Port(String)`: it is now only the
documented escape hatch for host implementations of the port traits,
mirroring `ChatError::External`, and carries no in-crate meaning. Chat wraps
the error as `ChatError::Governance(..)`; every `failure_event_code` string
is byte-identical to the pre-move value, verified by the untouched
assertions in `chat/tests/governance_cases` and the m0 fault suites.

The two dead `From` conversions between `EngineError` and `GovernanceError`
(no constructors, no consumers) were deleted, which also removed the only
edge from engine downward and made the chain acyclic.

## Consequences

- Both execution owners reach receipts, staging and approvals through one
  crate; AGENTKIT-141 can build the native journal on `DurableExecutionPort`
  without a chat dependency.
- `agentkit-chat` loses ~3.2k source lines (~15%) and its public API shrinks
  by 173 lines of surface; `agentkit-governance` grows by 204.
- Store coupling is now a single honest allowlist entry instead of being
  hidden inside chat; it disappears when AGENTKIT-131 lands.
- `check-arch` counts governance in the layer-3 rules (R2 chain, R12 file
  budget — `durable_execution.rs` at 1,024 lines carries the entry now).
