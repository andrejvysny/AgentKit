# WP-0.5 libkrun spike (Track B, macOS microVM helper)

This records the libkrun candidate of WP-0.5 for Milestone 0 of the Agent Sandbox spec, run on 2026-11-25. It covers the spec sections "Guest supervisor agentkit-sandboxd", "Guest protocol" and "macOS microVM helper", and the A-1, A-4, A-5 and A-11 equivalents. The Virtualization.framework candidate is in `wp-0.5-virtualization-framework.md`.

It is a throwaway spike. No AgentKit or OneMind code was changed. Code, scripts and raw results live outside git in `/Users/andrejvysny/workspace/onemind-workspace/spikes/libkrun/`, called `$SPIKE` below. Unless stated otherwise, every evidence file named here is in `$SPIKE/evidence/`.

## Setup

**Host**
- macOS 26.6.2 (25G83), Apple Silicon. The spec names macOS 15; not tested there.
- Rust 1.99.1. Xcode `/usr/bin/clang`. No Homebrew, no sudo, no system settings changed.
- OrbStack 29.8.2 provides Docker, used **only at build time** to turn the image into disks.

**Upstream artifacts** (downloaded from github.com/containers; `$SPIKE/dl/`, sha256 below)
- libkrun v1.19.8, tag commit `c4d283c2…`, source tarball `8896c711…`. libkrun publishes no binaries.
- libkrunfw v5.6.4, tag commit `b31ee303…`: source tarball `5b92b6f2…` and `libkrunfw-prebuilt-aarch64.tgz` `db126ed9…`. The prebuilt tarball holds `kernel.c`, the Linux 6.12.115 arm64 `Image` as a C array. No separate kernel download was needed.

**Builds**
- `libkrunfw.5.dylib`: `clang -shared kernel.c`, 0.7 s, no Linux toolchain needed.
- `libkrun.1.dylib`: `cargo build --release -p libkrun --no-default-features --features blk`, 7 s warm.
  - No `init-blob`: libkrun's own init is C cross-compiled for Linux, and the Makefile fetches a Debian sysroot from deb.debian.org, which was not approved.
  - No `net`: the feature is off, and the guest sees no virtio-net device (finding below). The binary was not inspected for net code.
  - Its crates came from crates.io.
- Raw kernel `Image`: dumped from the dylib with `krunfw_get_kernel()` (`tools/dump-kernel.c`, 18 lines of C).

**Helper** (`$SPIKE/helper`, crate `akvm`)
- `akvm-helper` runs one VM per process.
- `src/krun.rs` holds all FFI and 9 of the 10 `unsafe` blocks, each with a SAFETY comment. The 10th is `_exit` in `main.rs`.
- The bundle is laid out as `bundle/{bin,lib,share}`. The helper finds `@rpath/libkrun.1.dylib` through `LC_RPATH @executable_path/../lib`, with no `DYLD_*` variables.
- It is ad hoc signed with upstream's `hvf-entitlements.plist` (`com.apple.security.hypervisor`).

**VM shape**
- 2 vCPU, 2 GiB.
- Root disk `/dev/vda`: read-only raw ext4.
- Workspace disk `/dev/vdb`: read-write, sparse, raw ext4.
- One vsock device with TSI off (`krun_disable_implicit_vsock`, then `krun_add_vsock(ctx, 0)`), plus control port 1024. libkrun listens for it on a host unix socket (`krun_add_vsock_port2(…, listen=true)`).
- No network device.
- The console goes to a file (`krun_set_console_output`), which keeps stdin free for the parent pipe.

**Root disk** (`tools/build-disks.sh`, 32 lines)
- `docker export` of `agentkit-sandbox:v0-arm64`. Its id `sha256:f61e548f9b5a…` matches WP-0.2.
- Unpacked as root inside `debian:bookworm-slim` with `--network none`, then `mkfs.ext4 -O ^has_journal -d`. With no journal, nothing needs replaying on a read-only device.
- Result: 864 MiB file, 663 MB allocated. The whole build takes about 9 s.
- The guest init and an `/etc/agentkit/image-digest` file are added to the disk.

**Workspace disk**
- A 4 GiB ext4 template, `-E root_owner=1000:1000`, is made in the same container (2.2 MB allocated).
- Each workspace is an APFS clone of it (`cp -c`), 2.3 MB allocated.
- The host needs no e2fsprogs at run time, which was WP-0.3 finding 1.

**Guest PID 1** (`guest/agentkit-init.py`, 191 lines, Python from the image)
- Mounts `/proc`, `/sys` and tmpfs `/tmp`, `/run` and `/var/tmp`.
- Mounts `/dev/vdb` at `/mnt/ws` and bind-mounts its `workspace/` and `home/` onto `/workspace` and `/home/agent`.
- Serves the framed protocol on vsock 1024.
- Runs each command as uid/gid 1000 with `bash --noprofile --norc -c` and a fixed environment.
- On `shutdown`: sync, unmount, `reboot(POWER_OFF)`.
- Python stands in for the static musl `agentkit-sandboxd`: no Linux Rust target or toolchain image was approved.

**Protocol**
- A 4-byte big-endian length, then JSON, with a 1 MiB bound.
- `hello{protocol,generation}` is answered with `hello{protocol,generation,image_digest,guest_uptime_s}`. A mismatch gets an error and the channel closes.
- `exec{op,command}` returns `result{exit_code,stdout,stderr}`.
- `shutdown` returns `bye`.
- The generation reaches the guest on the kernel command line (`agentkit.generation=N`).

**Test parent** (`akvm-bench`, 195 lines, plus `tools/measure.py`, 249 lines)
- Stands in for OneMind and spawns the helper with a stdin pipe.
- Retries connect + hello every 2 ms. libkrun accepts the unix connection before the guest listens, and drops it if nothing listens on the vsock port.
- Times start → hello → first `id -u` result (must print `1000`), 50 further `true` commands and the stop.

## Findings that shape the verdict

1. **Boot configuration: libkrun hard-codes the command line for its bundled kernel.**
   - With libkrunfw, `krun_start_enter` always emits `… rootfstype=virtiofs rw … init=/init.krun`.
   - A block-device root is meant to go through libkrun's own init (`krun_set_root_disk_remount`), which needs the `init-blob` build.
   - Two ways work without it. Both were booted:
     - **External kernel (recommended):** pass the dumped `Image` to `krun_set_kernel(…, RAW, NULL, cmdline)` with our own command line, `root=/dev/vda rootfstype=ext4 ro init=/usr/sbin/agentkit-init agentkit.generation=N`. libkrunfw is then **not needed at run time**; a bundle without it boots (`no-libkrunfw-external.json`).
     - **Bundled libkrunfw plus parameter override:** pass `root=`, `rootfstype=`, `ro` and `init=` through `krun_set_env`. libkrun appends them quoted, and the kernel's last-one-wins parsing overrides the defaults. This works (`krunfw-mode-*.json`, `/proc/cmdline` shown there), but it relies on undocumented behaviour, and the quotes stay in `/proc/cmdline`. The guest has to strip them, a bug found and fixed in the spike.
2. **The VM lives inside the helper process.** `krun_start_enter` never returns. It runs vCPUs and devices on threads of the calling process and calls `exit()` when the guest powers off. So the VM cannot outlive the helper (A-11). Stop here means guest power-off (over vsock) or process exit. libkrun also offers `krun_get_shutdown_eventfd` (macOS/aarch64): it raises a GPIO power key in the guest, which PID 1 must handle. Untested. One helper process per VM, as the spec asks, is mandatory rather than a choice.
3. **libkrun's socket errors only log a warning.**
   - A control-socket path over `SUN_LEN` gave `WARN … Failed to create listening proxy … ENAMETOOLONG`, and the VM booted with no control channel. The parent waited until its 60 s timeout.
   - The listening socket is also left on disk after exit. `krun_add_vsock_port2(listen=true)` refuses an existing path with `EEXIST`.
   - The helper must check that the path is at most 103 bytes and unlink any stale socket before configuring.
4. **Memory comes back while running only in REPLACE mode.**
   - The balloon device and free-page reporting are on by default. The default macOS mode `madvise(MADV_FREE)`s reported pages, and footprint did not drop in 28 s without memory pressure.
   - With `krun_set_balloon_free_page_reporting_mode(REPLACE)` the VMM remaps the reported ranges, and a freed 1 GiB came back within about 8 s (`balloon.json`).
   - WP-0.3 and the VZ spike found no balloon at all.
5. **Hardened runtime with ad hoc signatures fails library validation.**
   - dyld refuses the ad hoc `libkrun.1.dylib` ("different Team IDs"), and the helper dies before `main` (`hardened.json`).
   - It works with `com.apple.security.cs.disable-library-validation` (`hardened-dlv*.json`), and should work once everything carries one Developer ID Team ID; not tested, since no Developer ID exists.
   - A third way avoids the dylib altogether: link the `libkrun` crate into the helper as an rlib. Not tried.

## Measurements

| Measure | Value | Evidence |
|---|---|---|
| Start to first command result, warm (helper spawn → `id -u` result at the parent) | **131–159 ms**, median 136 ms (9 runs) | `warm.json` |
| … first launch after the helper's signature changes (new cdhash, fresh workspace clone) | 382–398 ms (4 runs). Code-signing checks are the likely cause, not established: a fresh workspace with an unchanged signature started in 155 ms (`mem.json` vm2) | `cold.json`, `warm.json` run 1 |
| … first launch, hardened-runtime signature | 915 ms (1 run) | `hardened-dlv.json` |
| … first launch of a freshly copied bundle | 2.18 s (1 run) | `no-libkrunfw-external.json` |
| … very first boot after building | 1.13 s (1 run) | `a1-signing-sizes.txt` |
| Warm, REPLACE free-page reporting (the recommended setting) | 127–210 ms, median 139 ms (6 runs, first one on a fresh workspace); further command p50 0.49–1.04 ms, max 2.21 ms | `warm-fpr-replace.jsonl` |
| Truly cold page cache | **not measured**: `purge` needs sudo | — |
| Second VM started while the first runs | 155 ms | `mem.json` |
| Boot phases, warm (guest clock, verbose console) | kernel → `Run /usr/sbin/agentkit-init` 0.071 s · workspace mounted 0.116 s · listening 0.117 s. The guest answers hello at uptime 0.07–0.11 s, so libkrun setup before the kernel is about 15–30 ms | `console-verbose-boot.log` |
| Further command (`true`, parent round trip) | p50 0.51–0.58 ms, p90 0.57–0.80 ms, max 0.65–2.24 ms (10 × 50 runs; Python `subprocess` in the guest) | `warm.json`, `cold.json` |
| Graceful stop (`shutdown` frame → helper exited) | 2.2–7.4 ms | `warm.json`, `mem.json` |
| Parent pipe closed → helper gone, guest reachable | 25 ms | `a11.json` |
| Parent pipe closed, guest not reachable (hard-exit deadline) | 3.006 s, exit 75 | `a11.json` |
| Helper (= VM) process, idle, 2 GiB guest | RSS 159 MB, phys_footprint 154 MB (`vmmap --summary`). The `footprint -p` tool reports only 63 MB for the same process, so the two tools disagree by about 90 MB, probably over guest RAM mapped through Hypervisor.framework. All figures here use `vmmap`. REPLACE mode idle: RSS 159 MB, 153.5 MB | `mem.json`, `mem-fpr-replace-idle.txt` |
| Two VMs idle | 154 + 155 MB footprint (RSS 159 + 160 MB). Test parent 1 MB | `mem.json` |
| Guest touches and frees 1 GiB, default reporting | footprint 154 → 1229 MB, still 1229 MB 28 s later | `balloon.json` |
| Same, REPLACE reporting | 155 → 1229 → 680 MB (+3 s) → 224 MB (+8 s) → 168 MB (+28 s) | `balloon.json` |
| Memory returned on stop | yes: the process exits 2–7 ms after `shutdown`; `ps` shows no helper | `mem.json` |
| Helper binary | 403,520 bytes (arm64, stripped, LTO). Links only `@rpath/libkrun.1.dylib`, libiconv and libSystem | `otool -L` |
| `libkrun.1.dylib` | 4.85 MB. Links Hypervisor.framework, libiconv, libSystem | — |
| Kernel | `Image` 24.8 MB raw (13.1 MB gzip), **or** `libkrunfw.5.dylib` 24.9 MB | — |
| Root disk (v0) | 864 MiB raw, 663 MB allocated | — |
| Workspace disk | 4 GiB sparse; template 2.2 MB, clone 2.3 MB allocated, unchanged after use | — |
| Owned code | helper 409 lines of Rust (`krun.rs` 177, `main.rs` 166, `lib.rs` 58, `build.rs` 8). Guest init 191 lines of Python. Disk build 32 lines of shell, kernel dump 18 lines of C. Test parent 195 lines plus measure script 249 lines | `$SPIKE` |

**For comparison:**
- **WP-0.3 OpenShell, same Mac:** 4.9–5.4 s to first result, exec p50 3.6–4.6 ms, gateway + driver + supervisor 232 MB plus a 187 MB VM process.
- **VZ spike:**
  - 318–350 ms warm, exec p50 0.52–0.76 ms;
  - helper 17 MB plus VM service 147–150 MB footprint. Its report does not say whether that figure is `vmmap` phys_footprint or the `footprint` tool, which differ for libkrun (see above), so compare memory with that caveat;
  - no balloon;
  - stop 141–178 ms.

## In-guest facts

From `func-boot1.json` and `func-boot2.json`. Those boots ran on the first root-disk build (sha256 `99553520…`, unchanged across them). The disk was then rebuilt with a one-line init fix (finding 1), which gives the `54eaa17a…` used for all measurements.

**Mounts**
- `/dev/root / ext4 ro` (journal-less).
- `/dev/vdb` on `/workspace` and on `/home/agent`, both `rw,noatime`.
- tmpfs on `/tmp`, `/run` and `/var/tmp`.

**Ownership and identity**
- `/workspace` and `/home/agent` are `agent:agent 755`; `/tmp` and `/var/tmp` are `1777`. WP-0.3 finding 3 does not occur.
- `id`: `uid=1000(agent) gid=1000(agent) groups=1000(agent)`.

**Read-only root**
- `touch /usr/x` fails with `Read-only file system`.
- The host root image sha256 was `54eaa17a…` before and after the whole measurement suite (`a1-signing-sizes.txt`).

**Network**
- Interfaces: only `lo` and `dummy0`.
- TCP to `1.1.1.1:443`, to `127.0.0.1`, to `10.0.2.2` and to `192.168.64.1`: `ENETUNREACH`.
- UDP: `ENETUNREACH`.
- DNS: `EAI_AGAIN`.
- vsock to host CID 2 on an unmapped port **times out** (3 s); it is not refused.
- The libkrunfw kernel registers `PF_TSI`, but with TSI flags 0 the host proxies nothing.

**Devices**
- virtio ids: 5 (balloon), 4 (rng), 3 (console), 2, 2 (blk), 19 (vsock). No 1 (net).
- 2 CPUs, `MemTotal` 2032760 kB.

**Persistence**
- A 1 MiB random file in `/workspace`, plus `/home/agent/h.txt`, kept the same sha256 across stop and start.

## Per-criterion results (equivalents for this helper)

| ID | Result | Notes |
|---|---|---|
| WP-0.5 items | **Pass** | Boots the v0 root disk read-only as a block device; attaches a read-write sparse workspace disk; exchanges hello{protocol,generation} → hello{…,image_digest} over vsock; stops (guest power-off, helper exit 0). Plus exec as uid 1000. |
| A-1 | **Partial**; notarization **blocked** | Runs as a child of a parent from a bundle-shaped directory, from any cwd, with no `DYLD_*`, no Homebrew and no Docker at run time. Ad hoc signed; the hypervisor entitlement works ad hoc without hardened runtime. Hardened runtime needs one Team ID or `disable-library-validation` (finding 5). `spctl`: rejected (no Developer ID, D-26). Tested on macOS 26.6.2, not 15. |
| A-4 | **Pass** | Warm 131–159 ms; first launch after a re-sign or copy 0.38–2.2 s; target 15 s. Further command p50 about 0.5 ms; target 300 ms. |
| A-5 | **Pass** with REPLACE reporting | There is no gateway or driver process. Per VM: helper and VM are one process, 154 MB `vmmap` phys_footprint, 159 MB RSS (target 300 MB; either measure passes). Memory returns on stop. With REPLACE, freed guest memory also returns while running. |
| A-11 | **Pass** | Parent SIGKILL during a 60 s command: the helper sees the pipe close, sends a graceful `shutdown` over its own vsock connection, and the VM is gone in 25 ms. The progress file (4 lines) and the synced file survive. Helper SIGKILL: the VM dies with the process (25 ms). The parent sees EOF and reports `hold: unknown`. The synced file survives; the unsynced progress file does not exist after reboot (`a11-*-readback.json`). Guest unreachable: hard `_exit` at 3.0 s. No stray process afterwards (`pgrep`). There are only two layers here (parent, helper), so "gateway" and "driver" collapse into the helper. |

## How the kernel and boot configuration are supplied and updated

**Kernel.** libkrunfw 5.6.4 carries Linux 6.12.115, built from `github.com/libkrun/linux` tag `v5.6.4-6.12.115` with `config-libkrunfw_aarch64`. That config has virtio-blk, ext4, vsock, devtmpfs, `BINFMT_SCRIPT` and page reporting, and also the TSI patches, CAN, btrfs and ALSA. Two ways to ship it:
- **(recommended)** Ship the raw `Image` as an app resource and boot it with `krun_set_kernel`. To update the kernel, replace that file in an app update. A smaller config of our own would need a Linux build environment; a Docker container in CI would do, since libkrunfw's `MACOS_BUILDER=native` path needs Homebrew LLVM.
- Ship `libkrunfw.5.dylib`. libkrun `dlopen`s it by leaf name, which resolves through the helper's `LC_RPATH`. To update the kernel, swap the dylib (ABI 5).

**Command line.** In external mode it is entirely ours, built in `main.rs::boot_params`. In libkrunfw mode the defaults are fixed and overridden through `krun_set_env` (finding 1).

**Root filesystem.** One `root.ext4` per image generation, produced at build time from the OCI image. The generation is passed on the command line and checked in hello. Image updates swap the root disk; `/workspace` and `/home/agent` live on the separate workspace disk, so they survive an image update (A-3-friendly; not exercised).

**Alternative, documented but not tested: a virtiofs root directory.** `krun_set_root(dir)` with an unpacked rootfs on APFS, optionally `krun_add_virtiofs3(KRUN_FS_ROOT_TAG, …, read_only)`.
- The fixed command line runs `/init.krun`, so it needs `init-blob` or our own `/init.krun` inside the directory.
- Ownership on a host where every file belongs to the user goes through libkrun's macOS passthrough xattr `user.containers.override_stat` (seen in `src/devices/src/virtio/fs/macos/passthrough.rs`). The tooling that sets it was not examined. The block disk was tested instead, as the spec requires.

## License notes

**libkrun** is Apache-2.0. Its resolved dependency tree with `blk` is 84 crates (`libkrun-dep-licenses.txt`), all MIT, Apache-2.0, BSD-3-Clause (vmm-sys-util), ISC (libloading), Zlib (foldhash) or Unicode-3.0. A NOTICE is needed; there is no copyleft.

**libkrunfw** has two licenses:
- the library wrapper is LGPL-2.1-only;
- the kernel it embeds is GPL-2.0-only.

Shipping either the dylib or the extracted `Image` distributes a GPL-2.0 kernel binary. The OneMind distribution must then provide the corresponding source, or a written offer for it: the `libkrun/linux` tag tarball, `config-libkrunfw_aarch64` and the build scripts. The kernel runs in the guest as a separate work and does not affect the app's license.

The external-`Image` route ships no LGPL code at all. Shipping the LGPL dylib inside a sealed, signed `.app` raises the usual question of whether users can replace it. It is `dlopen`ed by leaf name, but replacing it breaks the bundle seal. That needs legal review.

The guest image licenses are WP-0.2's NOTICE.

## Risks

**libkrun upstream**
- API churn: the header announces a 2.0 API without implicit devices (issue #634). Pin to a tag and keep using the `disable_implicit_*` calls.
- The bundled-kernel command line is fixed (finding 1); use the external kernel.
- `krun_start_enter` takes over the process and calls `exit()`. Nothing in the helper can run cleanup after the VM stops. The helper must do all setup first and never keep state that needs flushing on exit.

**Silent misconfiguration.** An overlong socket path still boots a VM, just without a control channel (finding 3). The helper must validate every input and fail fast.

**Two writers on one workspace.** The helper takes no lock on `ws.ext4`, so two helpers on the same image would give the two-writer ext4 hazard WP-0.3 saw. Not tested here; a `flock` in the helper is needed.

**Durability**
- A hard kill loses guest page-cache writes. The supervisor must fsync anything it reports as done.
- The virtio-blk default on macOS is relaxed sync, not `F_FULLFSYNC`. `krun_add_disk3(…, KRUN_SYNC_FULL)` exists; not tested.
- Discard and TRIM back to the sparse file was not tested.

**No e2fsprogs anywhere at run time.** Workspace disks come from a template, so resizing or `fsck` would need e2fsprogs in the guest image. The v0 image has none.

**Kernel attack surface.** libkrunfw's config is broad (TSI, CAN, btrfs, sound). With no NIC and TSI off, the network surface is closed (verified), but a custom minimal config is worth considering.

**Signing.** First launch after a signature change or a copy costs 0.25–2 s (code-signing checks). Hardened runtime needs Team-ID-consistent signing (finding 5). Notarization was not tested.

**Spike shortcuts.** The guest responder is Python, which adds about 45 ms to boot. It does not reap orphans as PID 1, and it brings up no `lo`. The real `agentkit-sandboxd` replaces it.

## Recommendation

libkrun meets every WP-0.5 item and the A-4, A-5 and A-11 equivalents with wide margins:
- about 0.14 s to first result;
- sub-millisecond exec round trips;
- about 155 MB per VM;
- memory returned both while running (REPLACE) and on stop;
- the VM lifetime structurally bound to one process;
- about 410 lines of owned Rust.

Against the VZ spike, on the same host and with the same measurements:
- start is about 2.4× faster;
- stop is much faster;
- there is a working balloon;
- there is a single process per VM instead of helper plus XPC VM service.

The costs:
- a C ABI library that must be built from source, with its crate tree (finding 5 options);
- owning the kernel supply and its GPL source obligations. The VZ candidate needs a kernel too.

**Proposed choice:** libkrun, configured as follows:
- 1.19.x pinned, built from source with `blk` only (no `init-blob`, no `net`);
- external kernel `Image` with our own command line;
- REPLACE free-page reporting;
- read-only journal-less ext4 root;
- ext4 workspace from a cloned template;
- Team-ID signing, or the `libkrun` crate linked as an rlib to drop the dylib.

The final pick belongs to WP-0.6.

## Open questions
- Developer ID signing and notarization of a helper with `com.apple.security.hypervisor` plus a Team-ID-signed `libkrun.dylib` (or a static rlib): untested.
- macOS 15 behaviour (HVF GIC availability, entitlement): untested.
- Who builds and hosts the kernel `Image` and its GPL source: reuse libkrunfw's, or our own config?
- Durability policy for the workspace disk: `KRUN_SYNC_FULL` cost, and discard.

## Reproduce

All commands run from `$SPIKE`:
1. `tools/build-disks.sh` (needs Docker and the loaded `agentkit-sandbox:v0-arm64`).
2. Build libkrun: `cd src/libkrun-1.19.8-src/libkrun-1.19.8 && CARGO_TARGET_DIR=$SPIKE/build/libkrun-target cargo build --release -p libkrun --no-default-features --features blk`.
3. Build libkrunfw: `clang -fPIC -DABI_VERSION=5 -O2 -shared -Wl,-install_name,@rpath/libkrunfw.5.dylib -o libkrunfw.5.dylib kernel.c`.
4. Dump the kernel:
   - compile the tool: `clang -O2 -o dump-kernel tools/dump-kernel.c`;
   - run it: `./dump-kernel libkrunfw.5.dylib bundle/share/Image-6.12.115-krunfw5.6.4`.
5. Stage the dylibs in `bundle/lib`:
   - `libkrun.dylib` becomes `libkrun.1.dylib`, with `install_name_tool -id @rpath/libkrun.1.dylib`;
   - add `libkrunfw.5.dylib`;
   - `codesign -s -` both.
6. Build the helper: `cd helper && CARGO_TARGET_DIR=$SPIKE/build/helper-target cargo build --release`.
7. Install and sign: copy `akvm-helper` to `bundle/bin`, then `codesign --force -s - --entitlements tools/entitlements.plist bundle/bin/akvm-helper`.
8. Measure: `python3 -I tools/measure.py cold warm mem balloon a11 hardened`, and `build/helper-target/release/akvm-bench --id X [--krunfw] [--cmd …]` for single runs.

## Cleanup

`$SPIKE` went from 1.4 GB to 85 MB.
- `cargo clean --target-dir` was run on both target directories, removing 377 MiB and 28 MiB.
- Removed with plain `rm`:
  - `root.ext4` and `ws-template.ext4`;
  - every `run/*/ws.ext4`;
  - the extracted `kernel.c` (92 MB) and the libkrunfw build directory;
  - upstream's 100 MB `examples/snp-example-data-disk.img`. It is still in the downloaded tarball.
- Afterwards `pgrep -fl akvm-` and `ps` found no `akvm` or `krun` process, and `docker ps -a` showed no leftover export container.

Kept:
- source, scripts, evidence and the console logs;
- downloads (23 MB);
- the signed bundle with `Image` and the dylibs (52 MB).
