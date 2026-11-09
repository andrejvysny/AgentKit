# Sandbox macOS (Virtualization.framework) evidence — 2026-11-08

Host used for all development: Linux x86_64 (Ubuntu 24.04.5, kernel 7.0.0,
AMD Ryzen). **No Apple Silicon / Virtualization.framework host is available
here**, so every L3 check below is `NOT RUN`, honestly recorded per spec
§15.1/§15.2. Nothing in this file may be quoted as VM-isolation proof.

## What IS verified here (L1 only)

| Check | Where | Status |
|---|---|---|
| Adapter↔helper wire protocol, framing, caps | `sandbox-worker-apple/tests/parity.rs` (fake helper) | PASS |
| Provision gates: helper missing / probe unsupported → `sandbox_isolation_unavailable`, no fallback | `parity.rs::missing_helper_fails_closed` | PASS |
| Kernel + rootfs pinned-digest verification **before** helper spawn | `parity.rs::kernel_or_rootfs_digest_mismatch_refused_before_helper_spawn` | PASS |
| Manifest content: boot nonce, `agentkit.format_data=1` only on fresh generations, no network fields | `parity.rs::full_lifecycle_against_fake_helper` | PASS |
| Helper death pre-relay fails closed (no phantom Running VM) | `parity.rs::helper_death_during_launch_is_error_not_hang` | PASS |
| Termination evidence: reap-by-parent conclusive; inspect after terminate `Unknown` | `parity.rs::full_lifecycle_against_fake_helper` | PASS |
| Generation-dir double-provision refused | same | PASS |
| Worker `--backend apple-vz` wiring, fail-closed when feature not linked | `sandbox-worker` config/bin tests (`firecracker,apple` build) | PASS |

These are contract tests against a fake helper — §15.1 explicitly: not
isolation proof.

## NOT RUN (requires real macOS host — L3)

| ID | Scenario | Prereqs |
|---|---|---|
| T02 | macOS VM boot: real VZ helper, Apple Silicon guest kernel + ext4 rootfs | Apple Silicon, Xcode toolchain, signed helper |
| T03 | Entitlement enforcement: unsigned helper must fail `sandbox_isolation_unavailable` | same |
| T05b | Guest↔host vsock relay (CONNECT handshake through helper) with real guest agent | same |
| T23 | Crash/restart: `kill -9` helper ⇒ VM dies with it; terminate evidence conclusive; data disk not reformatted on next generation | same |
| T24b | Persistent disk survives VM restart, no silent format (`agentkit.format_data` absent) | same |
| T09/T10 | Package broker + OCI flows inside a real macOS VM | same |
| T12 | Cross-tenant negative matrix on macOS backend | same |

### Rerun recipe (Apple Silicon host, operator-authorized)

```sh
# 1. Build + sign the helper (entitlement is mandatory; README: apple/vz-helper)
swift build -c release --package-path apple/vz-helper
codesign --force --sign - --entitlements apple/vz-helper/entitlements.plist \
  apple/vz-helper/.build/release/agentkit-sandbox-vz-helper
# (unsigned variant kept to prove T03 refusal)

# 2. Build the Linux guest kernel + rootfs on any Linux box (scripts/sandbox-image)
#    — aarch64: build-kernel.sh --arch arm64 flow + musl guest agent —
#    then copy registry/{kernel.bin,rootfs.ext4}(+.sha256) to the mac host.

# 3. Adapter + worker (portable crates compile on macOS):
cd rust && cargo build --release -p agentkit-sandbox-worker --features apple

# 4. Worker boot (operator shell):
target/release/agentkit-sandbox-worker --uds /tmp/ak.sock \
  --storage-root ~/ak-storage --peer-uid $(id -u) \
  --backend apple-vz --kernel-path <kernel.bin> --rootfs-path <rootfs.ext4> \
  --helper-path apple/vz-helper/.build/release/agentkit-sandbox-vz-helper

# 5. Run the gated real tests (mirror of real_vm.rs):
AGENTKIT_REAL_VZ=1 cargo test -p agentkit-sandbox-worker-apple --test parity \
  -- --ignored --nocapture
```

Until step 5 executes green on real hardware, macOS results are **NOT RUN /
UNQUALIFIED** — CI skips them and must keep reporting them that way.

## Known constraints recorded during implementation

- `agentkit-sandbox-vz-helper` Swift source has **never been compiled**
  (no macOS toolchain here). Treat as design-grade until `swift build`
  succeeds on a mac; protocol/manifest shapes are pinned by the Rust-side
  L1 tests, which the Swift helper must satisfy.
- No host-side `mkfs.ext4` exists on macOS: data disks are zero-sparse
  files formatted **inside the guest initramfs** on first boot only
  (`agentkit.format_data=1` from the adapter; never for existing disks).
  The OCI importer likewise requires a Linux build host; images are
  content-addressed and digest-verified on the mac before boot.
- Helper trust model: admin-pinned binary, argv-only manifest, `env_clear`,
  same uid as worker, `com.apple.security.hypervisor` the sole entitlement.
  Termination proof is parent-side `waitpid` reaping (helper child), never
  a guest claim.
