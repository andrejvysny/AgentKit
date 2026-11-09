# Sandbox Worker — deployment, operations, disaster recovery

Code-coupled operations guide for the VM Sandbox Runtime
([ADR 0021](../adr/0021-vm-sandbox-runtime.md),
[spec](../specs/sandbox-runtime.md),
[invariant map](../sandbox-invariants.md)). All commands and file layouts
below match the code in `rust/crates/sandbox-worker*`; when they diverge,
the code wins — fix this file.

## 1. Deployment

### 1.1 Linux (Firecracker/KVM)

Prerequisites (verified, not assumed — the worker fails closed with
`IsolationUnavailable` if any is missing at create time, S05):

- `/dev/kvm` readable+writable for the worker user
  (`sudo usermod -aG kvm <user>` or a udev ACL; operator decision, not
  automatic).
- `firecracker` and `jailer` binaries pinned by digest and on `PATH`
  (or `AGENTKIT_SANDBOX_FC_BIN` / `AGENTKIT_SANDBOX_JAILER_BIN`).
- Cgroup v2 delegated directory for per-VM limits
  (`--cgroup-root`; without it the worker reports cgroup enforcement as
  unsupported in `BackendCapabilities` rather than claiming it).
- Trusted image registry built reproducibly by
  `scripts/sandbox-image/` with digests recorded in
  [`docs/baseline/`](../baseline/sandbox-l2-2026-11-08.md).

The worker runs **unprivileged** as its own user; there is no setuid path.
All VM state lives under `--storage-root`; nothing outside it is written
(S20). Host connects over the Unix socket at `--uds`; only uids listed by
`--peer-uid` (repeatable) pass `SO_PEERCRED` admission (S02).

```sh
agentkit-sandbox-worker \
  --uds /run/agentkit/worker.sock \
  --storage-root /var/lib/agentkit-sandbox \
  --peer-uid <host-uid> \
  --worker-id worker-a --revision <build-rev> \
  --install-fingerprint sha256:<64-hex> \
  --backend firecracker \
  --kernel-path /var/lib/agentkit-sandbox/registry/kernel.bin \
  --rootfs-path /var/lib/agentkit-sandbox/registry/rootfs.ext4 \
  --fc-bin /usr/local/bin/firecracker --jailer-bin /usr/local/bin/jailer \
  --cgroup-root /sys/fs/cgroup/agentkit
```

Every flag has an `AGENTKIT_SANDBOX_<FLAG>` environment fallback; both are
admin-owned (S20). The fake backend (`--backend fake`) exists for contract
tests only — never deploy it; it is reported honestly in `HelloAck`
capabilities.

### 1.2 Package broker (off by default)

`--broker-enabled` attaches the structured broker (PyPI/npm/crates.io only,
`curated-v1` policy, per-tenant budgets). Without it every guest fetch is
denied honestly (S04). `--insecure-test-origins` exists solely for the
local fake-registry tests and is loudly audited when set; production must
never set it.

### 1.3 macOS (Virtualization.framework)

Build and sign the Swift helper per [`apple/README.md`](../../apple/README.md)
(entitlements `com.apple.vm.hypervisor`/`Virtualization.framework` via
`entitlements.plist`), pin it with `--helper-path` (or
`AGENTKIT_SANDBOX_HELPER_PATH`) and run with `--backend apple-vz`. The
worker never talks to the framework directly; the helper is the only
privileged surface and validates that all paths sit under the private
storage root.

### 1.4 Image import

```sh
agentkit-sandbox-worker import-image \
  --registry <registry> --repository <repo> --digest sha256:<...> \
  --staging <dir> --images-dir <registry-dir> \
  --guest-agent-bin <bin> --guest-agent-digest sha256:<...> \
  --kernel-digest sha256:<...>
```

Fetch is by immutable digest; extraction runs under the strict tar policy
(absolute/`..`/symlink-escape/hardlink/device/fifo/setuid/sparse/bomb all
refused — `worker/src/oci.rs` tests) and the result is boot-validated
against the pinned kernel and guest agent before publication
(`--list` inspects the registry). Unsupported platforms fail explicitly;
nothing from an image executes on the host (S11).

## 2. Daily operations

- **Health/readiness**: `GetCapabilities` and `ReconcileQuery` over the
  UDS (the host bridge exposes both; `ReconcileQuery` returns the full
  inventory: sandboxes with `physical_state`, executions, reservations).
  Readiness ≠ security qualification; probe honesty is enforced
  (`BackendCapabilities { supported, enforced, evidence }`).
- **Audit**: `journal.jsonl` under the storage root records only
  ids/digests/decisions — never secrets or file content (S18). Transport
  emits `audit:` lines for peer rejections and broker enablement.
- **Evidence**: regenerate the T01–T24 acceptance matrix with
  `bun scripts/sandbox-evidence.mjs` (writes
  `docs/baseline/sandbox-evidence-<date>.md|json`). Hardware rows record
  `NOT RUN <reason>` when unmet — treat any regression there as a release
  blocker (S19).
- **Capacity**: per-tenant ceilings (`QuotaCeilings`: vcpus, RAM, disk,
  live VMs, global VMs) are refused transactionally at create
  (`ResourceExhausted`, no partial effects). Review after workload changes.

## 3. Restart & crash recovery (automatic, never optimistic)

On every start the worker replays its journal and quarantines any VM whose
launch lacks conclusive stop/destroy evidence (`physical=Unknown`,
`Quarantined`, executions → `Uncertain`). The first host `Hello` triggers
`reconcile_replayed`:

| Observed physical state | Action |
| --- | --- |
| Already terminated | Durable `VmStopped` evidence; quarantine cleared. |
| Still running | Survivor is reclaimed via terminate with evidence (the boot-nonce channel died with the old worker; command/process authority is never reused). Conclusive ⇒ `Stopped`, quarantine cleared; inconclusive ⇒ stays quarantined. |
| Backend unreachable | Stays quarantined; **all** operations refuse with `Quarantined`. |

A sandbox identity may only boot again as **exactly the next
`instance_generation`** (`CreateVm` with `generation+1` after proven
stop; anything else is `StaleGeneration`). Never hand-edit
`journal.jsonl` or the registry directory; the journal is append-only
durable state.

**Manual quarantine release**: there is none. If a sandbox is stuck
`Quarantined` (backend cannot prove termination — e.g. an external process
holds the hypervisor), an operator must make the process demonstrably gone
(kill it, verify via `ps`/pidfile under the generation directory), then
restart the worker so reconcile observes `Stopped`. Disk and pid resources
are retained while quarantined (S14) — that is the design, not a leak.

## 4. Disaster recovery

| Loss | Effect | Recovery |
| --- | --- | --- |
| Worker process crash | VMs survive; session channel (boot nonce) dies with worker. | Automatic (§3): reconcile reclaims, host re-creates at next generation. Guest-side command outcomes remain `Uncertain` until the host settles them on receipts — never silently `completed` (S07/S14). |
| Host crash / power loss | Same as above plus hypervisor processes gone. | Journal replay quarantines; backend probes prove absence; `VmStopped` clears. Ephemeral workspaces are rebuilt by the host; persistent disks re-attach (integrity-checked; uncertain state quarantines the disk). |
| Journal lost/corrupted mid-file | Torn tail records fail the digest chain. | Replay drops only the torn trailing record (checksum-verified); a wholesale-lost journal means the worker treats all storage roots as unreclaimed: resources stay quarantined until an operator verifies the disk/pid files under `storage-root/sandboxes/<tenant>/<sandbox>/gen-N/` by hand. Do not delete retained resources to "clear" quarantine. |
| Storage root disk failure | Data lost. | Restore `registry/` (images, kernel) from the digests in `docs/baseline/`; tenant data is unrecoverable without backups — back up `storage-root/sandboxes/<tenant>/` for persistent environments; verify digests after restore. |
| Broker cache corruption | Package fetches fail. | Delete `storage-root/broker-cache/` (cache, not authority); fetches re-verify digests regardless. |
| Worker binary replaced | `install_fingerprint` changes. | Host admission compares pinned fingerprints; an unexpected change must be treated as an incident (update the pin deliberately, never silently). |

Tenant offboarding: `DestroyVm` requires verified termination evidence;
deletion produces a `DeletionReceipt`; unresolved cleanup retains
resources quarantined rather than claiming erasure (T20/S14).

## 5. Troubleshooting quick table

| Symptom (error code) | Usual cause | Action |
| --- | --- | --- |
| `IsolationUnavailable` at create | No/insufficient `/dev/kvm`, missing fc/jailer, failed probe | Fix prerequisites (§1.1); confirm with `ReconcileQuery`/probe output. There is no fallback — do not add one. |
| `Quarantined` on all ops | Restart reconcile could not prove state | §3 manual release; check pidfile vs `ps` under the gen dir. |
| `StaleGeneration` | Client bound to an old instance; or bad reboot request | Host must re-admit at the live generation; generation-skip is a bug in the caller. |
| `TenantForbidden` | Cross-tenant access or unknown sandbox (deliberately indistinguishable) | Check the authenticated context, not the payload. |
| `PolicyDenied` on write with lease | Superseded/expired/foreign writer lease | Re-acquire writer (`WriterAcquire`); inspect contention in `audit:` lines (S09). |
| `PackageDenied` | Broker off, package/spec outside `curated-v1`, or unstructured request shape | Broker policy is admin config; guests have no NIC — this is the only install path (S04). |
| Guest handshake failure | Agent digest mismatch, unsupported protocol range, workspace disk missing | Re-import image with matching pinned digests; check gen dir contents. |
| `ProtocolIncompatible` on `Hello` | Worker/host version skew | Align protocol versions; rolling upgrade refuses incompatible pairs (spec §14). |

## 6. Security posture reminders

- Never grant the worker more privilege than an unprivileged service user;
  there is no supported privileged mode.
- `/dev/kvm` membership is an operator-authorized change (record it in the
  baseline doc).
- Real-VM evidence (L2 Linux, L3 macOS) gates releases; fake-backend
  passes are contract tests only (S19). The evidence generator records
  commands, versions and `NOT RUN` reasons — keep its output in Git.
