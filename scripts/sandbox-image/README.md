# sandbox-image — reproducible guest images

Produces the two trusted artifacts the real-VM harness and both backends
consume, into `registry/` (content + `.sha256` sidecars):

| Artifact | Builder | Notes |
|---|---|---|
| `kernel.bin` | `build-kernel.sh` | Direct-boot ELF (Firecracker + macOS `VZLinuxBootLoader`), initramfs embedded |
| `rootfs.ext4` | `build-rootfs.sh` | Trusted read-only root; `/init` execs the guest agent |

The private per-VM **data disk** is created by the backend at provision time
(ext4; formatted host-side by Firecracker, guest-side by the initramfs under
Virtualization.framework via `agentkit.format_data=1` — never silently
reformatted for persistent generations).

## Build

```sh
# 1. static guest agent (musl; no libc dependency in the guest)
cd ../../rust
cargo build --release -p agentkit-sandbox-guest --target x86_64-unknown-linux-musl \
  --target-dir target/guest
# (aarch64 guests: --target aarch64-unknown-linux-musl)

# 2. kernel (downloads source; --source <dir> for an audited local tree)
cd ../scripts/sandbox-image
./build-kernel.sh --version 6.12.8

# 3. rootfs (unprivileged mke2fs -d; no loop mounts, no root)
./build-rootfs.sh --agent-bin \
  ../../rust/target/guest/x86_64-unknown-linux-musl/release/agentkit-sandbox-guest
```

Image contents are pinned by sidecar digests; the worker/adapter verifies
sidecars before use and refuses mismatches. `agentkit-sandbox-guest-init.sh`
refreshes the agent inside an already-extracted rootfs tree (verifies
against `--agent-digest`).

OCI-imported images (`agentkit-sandbox-worker import-image`, spec §10.2)
land in the worker's admin `images_dir`, not here; this directory is the
from-source path and the real-VM harness default (`AGENTKIT_SANDBOX_IMAGE_DIR`
overrides).

## Real Linux VM gate (L2)

```sh
# once: unprivileged firecracker + jailer into ~/.local/bin (admin approval
# required for the download; KVM group membership is a privilege change and
# needs explicit operator authorization)
export AGENTKIT_SANDBOX_FC_BIN=~/.local/bin/firecracker
export AGENTKIT_SANDBOX_JAILER_BIN=~/.local/bin/jailer
export AGENTKIT_SANDBOX_IMAGE_DIR=$PWD/registry

AGENTKIT_REAL_VM=1 cargo test -p agentkit-sandbox-worker \
  --features firecracker --test real_vm -- --ignored --nocapture
```

Without `AGENTKIT_REAL_VM=1` (or when KVM/binaries are missing) the harness
prints an honest `NOT RUN` reason and skips; a skip is never a pass.
macOS Virtualization.framework hosts (L3) have no CI here — see
`docs/baseline/sandbox-macos-2026-11-08.md` for the exact NOT RUN matrix and
rerun recipe.
