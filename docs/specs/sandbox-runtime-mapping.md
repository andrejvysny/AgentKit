# Sandbox runtime — S01–S20 enforcement map

Companion to [`sandbox-runtime.md`](sandbox-runtime.md) and
[ADR 0021](../adr/0021-vm-sandbox-runtime.md). Every mandatory invariant
names its **enforcement owner** (the code that refuses the violation) and
its **tests**. Phases: `C` = contracts crate (done), `W` = worker, `G` =
guest agent, `H` = host/coding-pack, `B` = broker/importer, `P` = platform
helper. A row is only *validated* when its listed tests pass at the phase's
evidence level; `docs/specs/sandbox-runtime-evidence.md` tracks actual runs.

| ID | Enforcement owner | Tests | Phase |
|---|---|---|---|
| S01 one tenant/sandbox/generation/policy/resources per VM | `spec::VerifiedSandboxSpec` + `api::VerifiedLaunchSpec` (ids required, clamped); worker `Registry` refuses ops whose envelope gen/policy differ from record | `spec.rs` tests, `worker/tests/lifecycle.rs` | C+W |
| S02 tenant from authenticated context only | `grant::AuthContext` (no serde, host-constructed); `SandboxSpec` wire omits tenant (`deny_unknown_fields` rejects injection); worker checks grant vs authoritative | `spec.rs::tenant_is_injected_wire_has_no_tenant_field`, `grant.rs::grant_verification_matrix`, `worker/tests/tenant_denial.rs` (T01) | C+H |
| S03 no host mounts/devices in guests | `spec::SandboxSpec.host_mounts` non-empty ⇒ `HostMountsForbidden`; firecracker backend builds jailer chroot with only kernel/rootfs/disk/api-sock; VZ helper validates paths under private root | `spec.rs::host_mounts_are_rejected_not_ignored`, `sandbox-worker-fc/src/backend.rs` unit tests (jailer identity, chroot layout), T03 real-VM | C+P |
| S04 no guest network except broker | backend config: no net device ever attached (firecracker: no network iface call; VZ: no `VZVirtioNetworkDeviceConfiguration`); broker serves only `PackageFetchRequest` | `messages.rs` catalogs contain no URL ops; broker tests (T04–T06) | W+B+P |
| S05 fail closed without verified virtualization | `worker::backend::firecracker::probe()` gate + `HostToWorker::CreateVm` preflight ⇒ `IsolationUnavailable`; no fallback branches exist | `sandbox-worker-fc/src/probe.rs::probe_fails_closed_without_binaries` + `worker/tests/real_vm.rs` hardware gate (T02), T02 | W+P |
| S06 all sandbox-bound tool I/O via service, no host fallback | coding-pack `WorkspaceStorage`/`CommandRunner` seam: VM target constructs only `GuestStorage`; error paths return errors, never host calls | `toolpacks-coding/tests/vm_routing.rs` + `toolpacks-coding/tests/sandbox_escape.rs`, T17 | H |
| S07 external mutations fenced by immutable command identity before dispatch | Host `admit_execution` + `WorkspaceGrant.intended_request_digest` + worker journal `admitted→dispatched` | `grant.rs::grant_verification_matrix`, worker journal tests (T11) | C+H+W |
| S08 worker/guest cannot mint grants; subagent ≤ parent∩role∩tenant∩policy | Grants minted only by host `SandboxService`; `WorkspaceGrant::verify` full-field check; subagent delegation intersects ceilings (`host::subagents`) | `grant.rs` matrix, `host/tests/subagents_worker.rs` (T16, live worker) | C+H |
| S09 one admitted writer per sandbox | `WriterLeaseToken` (generation+epoch bound), worker-authoritative `WriterAcquire` epoch admission (durable `WriterAcquired` journal record), mutating grants carry the live token, worker refuses mount of same disk twice | `worker/tests/writer_lease.rs` (no-lease/supersession/stale epoch/holder/expiry + journal-durable admission) + `host/tests/subagents_worker.rs` parent/child contention (T16, live worker) | H+W |
| S10 guest env allowlist only | `grant::GuestEnvironment` deny-list + shape checks (compile-time impossible to pass raw map) | `grant.rs::environment_blocks_host_credentials`, guest exec test (T09/T12) | C+G |
| S11 OCI content never executes host-side | importer: by-digest fetch, no entrypoint/RUN; extraction in disposable VM; hardened parser as second layer | `worker/src/oci.rs` hostile-tar unit tests (`rejects_absolute_escape_and_dot_segments`, `rejects_symlink_escape_and_absolute_target`, `rejects_setuid_and_device_and_fifo`, `decompression_bomb_is_capped`) + `worker/tests/oci_import.rs` (T07), boot test (T08) | B |
| S12 import/export verify path/ownership/size/type/digest/limits/grant | `api::validate_stage_relative_path`, `StageManifest` digests, worker staging checks, guest `openat2` writes | stage tests (T13/T14) | C+W+G |
| S13 quotas incl. vcpu/ram/disk/pids/vm/cmd/logs/broker | `limits::ResourceCeilings` clamp at admission; worker reservation table transactional; broker budgets per tenant/window | spec clamp tests, worker quota tests (T12) | C+W+B |
| S14 termination ≠ rollback proof; unknown quarantines | `state::CommandState::proves_not_running` (guest claims excluded), `Uncertain`/`Quarantined` terminals, `RetryClass::UncertainReconcile` | `state_machine_matrix.rs::guest_claims_never_count_as_termination_proof`, crash tests (T11/T20) | C+H+W |
| S15 persistent sandboxes can't transfer; deletion fenced | host registry owner checks (`GrantExpectation` tenant+owner), destroy requires `SandboxManage` + verified termination | `host/tests/binding_sqlite.rs` + `worker/tests/lifecycle.rs` (T15/T20) | H |
| S16 every frame/path/env/log bounded, versioned, duplicate-suppressed | `protocol` frame cap + version + `DuplicateTable`/`SequenceGuard`; `GuestPath`/`GuestEnvironment` caps | `protocol_adversarial.rs` (T18/T24 L1) | C |
| S17 guest output is data, never authority | worker maps guest messages through fixed catalogs; status consumed only as observations; host receipts record `reported` vs `observed` separately | `messages.rs` strictness tests, worker event tests (T09) | C+W |
| S18 audit records exclude secrets/content | worker/host audit structs take ids/digests only; env/stdout previews redacted before logging | audit redaction tests (W) | H+W |
| S19 no silent platform skip | `CapabilityManifest`/`BackendCapabilities` `{supported,enforced,evidence}`; probe-gated admission | probe + parity matrix tests (T02/T17) | W+P |
| S20 no privileged daemon accepts raw paths/mounts/image paths | launcher accepts only worker-derived identifiers (sandbox_id/generation); all paths derived under worker root (`VerifiedLaunchSpec` carries digests, not paths) | launcher CLI validation tests (P) | W+P |

## T01–T24 coverage index

T-id → implementing test file is maintained in
[`sandbox-runtime-evidence.md`](sandbox-runtime-evidence.md) together with
per-run evidence (commands, versions, digests, outcomes, skip reasons).
