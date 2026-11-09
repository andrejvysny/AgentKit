# ADR 0021 — VM sandbox runtime: isolated guest execution for sandbox-bound coding tools

**Status:** proposed → M0 contracts implemented (2026-11-08). Hardware qualification pending (see `docs/specs/sandbox-runtime-evidence.md`).
**Spec:** [`docs/specs/sandbox-runtime.md`](../specs/sandbox-runtime.md) (S01–S20, T01–T24, M0–M7).
**Contract impact:** additive. No existing public API/tool behavior changes for sessions that do not select the VM target. `CONTRACT_VERSION`/sqlite `SCHEMA_VERSION` bumped only for the additive sandbox tables; legacy descriptors keep reading unchanged. New crates: `agentkit-sandbox-contracts`, `agentkit-sandbox-worker`, `agentkit-sandbox-guest`.

## Problem

Every sandbox-bound coding path today performs **host** I/O: `fs_tools`/`search_tools`/`git_tool` read the host filesystem and `exec_tools` spawns host processes under a Seatbelt profile (macOS) or refuse (`shell_isolation_unavailable`) elsewhere ([ADR 0018](0018-coding-tool-pack.md), `rust/crates/toolpacks-coding/src/{fs_tools.rs,search_tools.rs,git_tool.rs,exec_tools.rs,sandbox.rs}`). Seatbelt is a host-process constraint, not VM isolation, and `SandboxMode::Off` is a legitimate legacy policy. A multi-tenant backend that runs untrusted, model-generated code needs hardware-virtualized isolation on both supported platforms, with **no** silent fallback to host shell, Docker, or Seatbelt-only execution (spec S05, S06).

Reusing the host-side machinery for guests is also unsound: process-group ownership and lock-dir leases ([ADR 0020](0020-execution-ownership-and-recovery.md)) prove effects ended *within the current OS/namespace*, which a guest VM breaks. The trusted computing base must therefore split cleanly:

- **Trusted:** AgentKit Host (sessions/tasks/approvals/receipts/governance — [ADR 0019](0019-human-approval-plan-mode-and-side-effect-receipts.md)), the worker control binary, the VM launcher, the hypervisor.
- **Untrusted:** guest OS, guest agent, all guest code. Guest responses are observations, never authorization.

## Decision

### 1. Process topology (co-located, privilege-separated)

```
AgentKit Host (existing) ──UDS──▶ agentkit-sandbox-worker (unprivileged daemon)
                                     │  own journal, quota-physical gate
                                     ├──▶ VMM launcher helper (Firecracker/jailer on Linux; Swift VZ helper on macOS)
                                     └──▶ vsock ⇄ guest agent (untrusted)
```

Host and worker are separate OS identities with separate credential sets; the UDS is peer-credential-checked (`SO_PEERCRED` on Linux, `getpeereid` on macOS). `WorkerTransport` is an interface so a future mTLS transport can reuse the protocol without re-hardening. The worker journals *physical* VM/command inventory for crash reconciliation but never schedules agent tasks (the Host owns logical admission).

### 2. Contracts first (`agentkit-sandbox-contracts`)

`rust/crates/sandbox-contracts/src/` defines: identity newtypes with strict parse/serde (`ids.rs`), `SandboxSpec` with **server-clamped** resources and `host_mounts` **rejected not ignored** (`spec.rs`), effective policy + content-addressed `PolicyDigest` + frozen `RegistryPolicy` (`policy.rs`), the §7 state machine as a frozen transition table (`state.rs`), the §6.5 error contract with retry classes (`error.rs`), guest path validation (`path.rs`), host-minted `WorkspaceGrant`/`GuestEnvironment` (`grant.rs`), the bounded frame protocol with digest/duplicate/sequence guards (`protocol.rs`, `messages.rs`), and the `SandboxManager`/`SandboxWorkspaceIo`/`SandboxBackend` traits (`api.rs`). All catalogs use `deny_unknown_fields`; every limit is a named constant in `limits.rs`.

### 3. Tool routing — one algorithm implementation, two byte sources

`SandboxWorkspaceIo` is the only sandbox-bound I/O path. The coding pack keeps exactly **one** implementation of each algorithm (edit engine, search walker, git argv sanitization, output caps, truncation notices). A new `WorkspaceStorage`/`CommandRunner` seam in `toolpacks-coding` selects between:

- `HostStorage` — the current direct host code path (unchanged semantics, default when target is host), and
- `GuestStorage` — drives `SandboxWorkspaceIo` RPCs; algorithms run in the trusted host over guest-provided bytes with CAS (`expected_sha256`) writes that the guest agent executes as guarded atomic rename (`openat2` `RESOLVE_BENEATH|NO_MAGICLINKS|NO_SYMLINKS`, temp+rename in-guest). Guest files are never materialized under a host path.

Routing is decided by a versioned session binding (§4); **a VM-target session has no host-I/O code path reachable**: the `WorkspaceStorage`/`CommandRunner` for a VM target is always the guest implementation, including after timeouts/errors (S06). `SandboxMode::{Required,Off}` keeps its legacy meaning for host-mode sessions only; VM sessions are a separate named target that cannot downgrade to `Off`.

### 4. Session binding and backward compatibility

`rust/crates/host/src/session/descriptor.rs` gains an optional versioned `sandbox` block (`target: "vm_sandbox"`, `sandbox_id`, `policy_digest`, `tool_grants`) alongside the legacy `{workspace:{root,trusted}}`. Old descriptors read unchanged; new sessions must set `target` explicitly — never flipped mid-run. `tenant_id`/`principal_id` come from authenticated host context; the worker checks the grant on every operation, never trusting client or model parameters (S02). N-API gains additive sandbox options on `registerCodingTools` (`executionTarget`, `sandboxId`); no second TS execution engine, no behavior change when absent.

### 5. Execution: fenced, journaled, uncertainty-preserving

Every mutation gets a durable Host receipt (existing `admit_execution`) **plus** sandbox- and generation-fenced worker journal records: `admitted → dispatched → acked → spawned → exited → termination_observed → cleanup_verified`, with `uncertain`/`quarantined` terminals. Guest-claimed `exited` is never proof by itself; the worker independently observes the VM/hypervisor process (`proves_not_running()` in `state.rs` collapses guest-only states). Duplicate `execution_id` with identical digest replays the recorded result; conflicting digest errors; unknown completion is `uncertain` — never auto-replayed (S07, S14).

### 5.1 Restart recovery and the generation lifecycle

The worker's registry is reconstructed from its journal on every start; a replayed record whose VM launch lacks conclusive stop/destroy evidence comes back `physical=Unknown` **and quarantined** — every operation on it refuses (`Quarantined`) until the worker independently proves the physical state. The proof pass (`WorkerService::reconcile_replayed`) is triggered automatically by the first authenticated host `Hello` and is deliberately **not optimistic**: a survivor VM (still running but whose boot-nonce channel died with the old worker process) is reclaimed via the normal terminate path with durable `VmStopped` evidence; conclusive evidence is the only thing that clears a quarantine (`registry.clear_quarantine`); an unprovable state stays quarantined indefinitely. Re-launching a sandbox identity is legal only as **exactly the next `instance_generation`** after a proven-stopped one (quota and mount handoff happen atomically in `apply_vm_created`); in-place reuse and generation-skip are `StaleGeneration`. Every envelope-bearing operation (create/stop/destroy/inspect/writer-acquire) is additionally pinned to the registry generation by `service::check_generation`, so a stale or forged envelope can never act on an instance it did not bind (S01, S14, spec §8.3/§11.4). Harnesses: `worker/tests/lifecycle.rs::worker_restart_reconciles_then_reboots_new_generation`, `worker/tests/soak.rs` crash phase, `fault_injection.rs`.

### 6. Network: deny-by-default with structured broker

Guests get **no NIC**. Package installs flow through the host worker broker over vsock: only `PackageFetchRequest`-shaped requests (ecosystem + package + version spec [+ pinned digest]) against a frozen `RegistryPolicy` (PyPI/npm/crates.io, `curated-v1`) are served, revalidated at every redirect, budgeted per tenant/window. There is no `CONNECT` tunnel or URL-fetch method in `packages_only` (S04).

### 7. Images: OCI → VM rootfs, conversion isolated

Import by immutable digest only; media-type/arch/size validation; layer/whiteout reconstruction under strict tar policy (absolute/`..`/symlink/hardlink/device/sparse/setuid rejected) executed inside a disposable VM (or worker-side parser hardened as a defense-in-depth double), with the trusted kernel + guest agent grafted on a controlled init; boot-validated; published content-addressed with full provenance (`image_digest`, kernel/rootfs digests, conversion version). No Dockerfile `RUN`/entrypoint execution ever on the host (S11).

### 8. Legacy host mode preserved

Existing host workspace/Seatbelt APIs keep behaving exactly as before for explicitly selected host sessions (T17). VM sessions cannot flip to host; tenant-controlled endpoints prohibit `sandbox: off`. The high-risk command guard and approval model remain authoritative; sandbox policy composes as an **additional AND gate** ([ADR 0011](0011-tool-governance.md), [ADR 0019](0019-human-approval-plan-mode-and-side-effect-receipts.md)).

## Consequences

- New durable state: sandbox registry + leases + reservations live in the existing store (schema migration); worker keeps a physically separate journal for independent crash discovery, reconciled against host receipts.
- `run_tests` keeps exact-configured-command matching; arbitrary shell text is allowed *inside the guest* only after authorization (the VM, not text scanning, is the boundary).
- Co-location is a deployment compromise, stated in the operator guide: a hypervisor escape could reach the backend; dedicated nodes recommended for high-assurance tenants.
- Platform parity is honest: capability manifests carry `{supported, enforced, evidence}`; Firecracker and Virtualization.framework differ on cgroup semantics and startup time; unsupported capabilities error out, never silently degrade (S19).
- The real-VM evidence (L2 Linux, L3 macOS) is a release gate, not a development prerequisite; fake-backend tests are contract tests only and are labeled as such in evidence reports.
