# WP-0.5 Virtualization.framework spike (Track B, macOS microVM helper)

This records the Apple Virtualization.framework candidate of WP-0.5 ("libkrun and Virtualization.framework spikes") for Milestone 0 of the Agent Sandbox spec. It was run on 2026-11-25. It covers the spec sections "Track B: guest supervisor", "Guest protocol" and "macOS microVM helper", and the A-1, A-4, A-5 and A-11 equivalents from "Exit criteria for Milestone 0". The libkrun candidate was spiked in parallel by another agent and is reported separately.

It is a throwaway spike. No AgentKit or OneMind code was changed. Spike code, scripts and raw logs live outside git in `/Users/andrejvysny/workspace/onemind-workspace/spikes/vz/`, called `$SPIKE` below. Unless stated otherwise, logs named here are in `$SPIKE/logs/`.

## Setup

**Host**
- macOS 26.6.2 (25G83) on Apple Silicon. The spec names macOS 15; nothing was run on 15.
- Rust 1.99.1. No Homebrew installs, no sudo, no system settings changed.
- OrbStack was used only to build the disks (below), never on a timed path.
- A pre-existing `com.apple.Virtualization.VirtualMachine` process (PID 69087) was excluded by snapshotting PIDs before each run. It started the day before; that it belongs to OrbStack is an assumption from the start time.
- The parallel libkrun spike was running benchmarks at the same time, so timings carry some noise.

**Helper** (`$SPIKE/helper`, crate `vz-helper`, Rust)
- Uses `objc2 0.6.5`, `objc2-virtualization 0.3.2`, `objc2-foundation 0.3.2`, `block2 0.6.2`, `dispatch2 0.3.1`, `libc` and `serde_json`. That is 22 crates in the lockfile, all from crates.io.
- The Rust bindings did not block anything, so Swift was not needed.
- One helper process runs one VM. The helper owns a private serial dispatch queue, and every VM call goes through it.
- The parent drives the helper over the stdin pipe, one JSON request per line, and reads JSON events on stdout. stdin EOF means the parent is gone.
- Built with `MACOSX_DEPLOYMENT_TARGET=15.0` (`minos 15.0`), release, LTO, stripped. Ad hoc signed by `$SPIKE/scripts/build.sh`.

**VM shape**
- `VZLinuxBootLoader` with the kernel, initramfs and command line; generic platform; 2 vCPU; 2 GiB.
- `vda`: the root disk, `readOnly: true`, caching `Cached`, sync `None`.
- `vdb`: the workspace disk, read-write, caching `Automatic`, sync `Full` (the guest's flushes become host fsyncs).
- One virtio vsock device. Control uses port 1024. The egress port was not built.
- A virtio console attached to a log file (`hvc0`), virtio entropy, and a traditional virtio memory balloon.
- **No network device.**

**Kernel** (owner-approved download)
- Debian `linux-image-6.12.117+deb13-cloud-arm64_6.12.117-1_arm64.deb` from deb.debian.org.
- Its SHA-256 `cc33e288…c22f` matches the `Packages` entry of trixie (13.8). The InRelease GPG signature was not checked.
- `/boot/vmlinuz-*` is already an uncompressed arm64 `Image` (30.9 MB), which VZ boots directly.
- The cloud flavour builds ext4 and virtio-pci in, but `virtio_blk`, `vsock`, `vmw_vsock_virtio_transport(_common)`, `virtio_balloon` and `virtio-rng` are modules, so an initramfs is required.

**Initramfs** (`$SPIKE/scripts/mkinitrd.sh`, 1.96 MB gzip newc)
- busybox and its glibc libraries from the local `busybox:latest` image.
- The six modules above, decompressed from `.ko.xz`.
- `/init`, a 31-line shell script:
  - loads the modules;
  - mounts `vda` as `ro,noload` and `vdb` as `rw` on `/workspace`;
  - bind-mounts `/workspace/.sandbox/home` on `/home/agent`;
  - puts tmpfs on `/tmp` (1777) and `/run`;
  - runs `switch_root` to `/usr/bin/python3 -I -S /run/agent/responder.py` as PID 1.

**Guest responder** (`$SPIKE/guest/responder.py`, 148 lines, stdlib only)
- A stand-in for `agentkit-sandboxd`. There is no musl cross target on this host, so a static Rust guest binary was not built.
- Speaks the spec's framing: a 4-byte big-endian length, then a JSON body of at most 1 MiB.
- `hello {protocol, generation}` returns `hello {protocol, generation, image_digest}`. A mismatch closes the channel.
- `exec {op, command, cwd, deadline_ms, output_limit}` runs `bash --noprofile --norc -c` as uid/gid 1000 with no extra groups, in a new session. It kills the session afterwards and reaps orphans as PID 1.
- `shutdown` does the following, then replies `bye`:
  - SIGKILLs everything;
  - syncs;
  - unmounts `/home/agent` and `/workspace`;
  - powers off with `reboot(POWER_OFF)`.
- The generation and image digest come from the kernel command line (`agentkit.gen=`, `agentkit.image=`), which the helper writes.

**Disks** (`$SPIKE/scripts/mkdisks.sh`, in a throwaway `debian:bookworm-slim` container, `--network none`)
- `docker export` of `agentkit-sandbox:v0-arm64` (id `f61e548f…`, the WP-0.2 v0).
- `/.dockerenv` is removed and `hosts`/`resolv.conf` emptied.
- Root disk: `mkfs.ext4 -d` into a 1100 MiB raw image, with a fixed UUID and hash seed and `E2FSPROGS_FAKE_TIME` set to the image epoch. 709 MB is used and the file allocates 663 MB.
- Workspace template: an 8 GiB sparse ext4 (`root_owner=1000:1000`, lazy init) that allocates 4.3 MB on APFS. Each workspace is an APFS clone of it (`cp -c`, 2 ms), so creating a workspace needs no host e2fsprogs.

**Parent stand-in** (`$SPIKE/scripts/driver.py`)
- Spawns the helper and timestamps from just before spawn.
- Sends `exec` requests and closes the pipe at the end.
- Finds the new VM service PID by diffing `pgrep` snapshots.

## Findings that shape the verdict

1. **The entitlement is `com.apple.security.virtualization`, and ad hoc signing is enough.**
   - With no entitlement, and with `com.apple.security.hypervisor` alone, `validateWithError` fails with: *"Invalid virtual machine configuration. The process doesn't have the "com.apple.security.virtualization" entitlement."* (`sign-none.txt`, `sign-hypervisor.txt`)
   - `virtualization` alone boots.
   - The hypervisor entitlement is neither needed nor sufficient. Presumably the framework's XPC service holds the hypervisor access; not verified.
   - Hardened runtime (`codesign -o runtime`) plus the entitlement, ad hoc, also boots (`a1-signing.txt`). No provisioning profile is needed.
2. **The VM lives in an Apple XPC service process, not in the helper.**
   - Guest RAM is in `com.apple.Virtualization.VirtualMachine`, a child of launchd.
   - That process dies with its client: SIGKILL of the helper removed it within 0.08 s. This is the property macOS otherwise lacks (no parent-death signal).
3. **Virtualization.framework locks disk images itself.**
   - A `flock` held by the helper on the workspace image made VM start fail with *"The storage device attachment is invalid."* (`sign-virtualization.txt`)
   - A second helper on the same workspace, with the helper's own lock disabled, fails the same way (`lock-vz-native.txt`). The two-writer ext4 hazard WP-0.3 found under OpenShell cannot happen here.
   - The read-only root disk is shared by two VMs without trouble.
   - The spike also takes `flock` on a sidecar `<workspace>.lock`, so a second helper is refused before booting (`lock-sidecar.txt`).
4. **The balloon does not give memory back to the host, as observed.**
   - Setting `targetVirtualMachineMemorySize` to 512 MiB inflates the guest balloon: guest MemFree dropped from about 1.86 GB to 0.28 GB.
   - The VM process footprint stayed at 1796 MB for 20 s, and `footprint` shows 0 B reclaimable (`a5-balloon.txt`).
   - Memory returns to the host only when the VM stops. This is the same as OpenShell.
   - This was a single run with a 20 s window and no host memory pressure. It is an observation on macOS 26.6.2, not a confirmed platform property.
5. **Docker is needed only to build the root disk.** It is off every runtime path. The production path is below.

## Measurements

| Measure | Value | Evidence |
|---|---|---|
| Start to first command result, warm (helper spawn → `true` result at the parent) | **318–350 ms**, median 328 ms (5 runs) | `a4-timing.txt` |
| … after the binary's signature changes (new cdhash; first-launch code-signing checks) | 615–636 ms (3 runs); about 275 ms is exec overhead before `main` | `a4-timing.txt` |
| … first VM of the session, first build | 995 ms at the parent, 425 ms inside the helper (the parent figure includes one `pgrep`) | `first-events.jsonl` |
| Boot phases, warm (helper clock) | configured 9–11 ms · VZ `start` done 78–83 ms · vsock connected 311–343 ms · hello +0.3 ms | `t-warm*-events.jsonl` |
| Guest timeline (uptime) | modules loaded 0.13–0.14 s · `switch_root` 0.17–0.18 s · responder listening 0.19–0.22 s | `*-console.log` |
| Truly cold page cache | **not measured**: `purge` needs sudo | — |
| Further command (`true`, parent round trip) | p50 0.52–0.76 ms, p90 0.60–1.06 ms, max 0.99–3.44 ms (8 × 50 runs) | `a4-timing.txt` |
| Graceful stop (parent closes pipe → VM process gone) | 141–178 ms | `a4-timing.txt` |
| Helper process, idle | RSS 27 MB, footprint 17 MB | `a5-memory.txt` |
| VM service process, idle (2 GiB guest) | RSS 193–199 MB, footprint 147–150 MB | `a5-two-idle.txt` |
| Two VMs idle | helpers 17 + 17 MB, VMs 147 + 150 MB footprint (RSS 27 + 27 + 193 + 199 = 446 MB) | `a5-two-idle.txt` |
| After the guest touches and frees 1.5 GiB | VM footprint 1691 MB; stays until stop (balloon: finding 4) | `a5-memory.txt`, `a5-balloon.txt` |
| Memory returned on stop | yes; both helpers and both VM processes gone 149–179 ms after the pipe closed | `a5-memory.txt`, `a5-two-idle.txt` |
| Helper binary | 519 KB (arm64, stripped, LTO). Links Virtualization, Foundation, CoreFoundation, libobjc and **AppKit** (pulled by default crate features) | `a1-signing.txt` |
| Kernel + initramfs | 30.9 MB `Image` + 1.96 MB initrd | — |
| Root disk (v0) | 1100 MiB raw, 663 MB allocated; 709 MB used in ext4 | — |
| Workspace disk | 8 GiB sparse, 4.3 MB allocated when new; clone 2 ms | — |
| Owned code | helper 531 lines of Rust (rustfmt'd; 19 lines mention `unsafe`: FFI calls, two `Send` wrappers, `flock`/`dup`/`shutdown`). Guest responder 148 lines of Python, init 31 lines of shell. Build scripts 36 lines. Test parent 127 lines | `$SPIKE` |

**For comparison**, WP-0.3 measured OpenShell on the same Mac:
- 4.9–5.4 s from start to first result, mostly the e2fsprogs shim.
- Exec p50 3.6–4.6 ms.
- Idle RSS: gateway + driver + supervisor 232 MB, plus a 187 MB VM process.

## In-guest facts (`guest-facts.txt`, `persist.txt`)

- `id` gives `uid=1000(agent) gid=1000(agent) groups=1000(agent)`. The kernel is `6.12.117+deb13-cloud-arm64`.
- Ownership is preserved from the image, unlike OpenShell C1/C2 in WP-0.3 finding 3:
  - `/workspace` and `/home/agent` are `agent:agent`;
  - `/tmp` and `/var/tmp` are `drwxrwxrwt`;
  - `/etc/shadow` is `root:shadow 0640`.
- Root is read-only: writing `/` fails with `Read-only file system`.
- Mounts: `/dev/vda / ext4 ro,norecovery`, `/dev/vdb /workspace ext4 rw`, and the same disk on `/home/agent`.
- Networking:
  - `/sys/class/net` has only `lo`;
  - TCP to `1.1.1.1:443` returns `ENETUNREACH` (101);
  - `getent hosts pypi.org` fails.
- Persistence: a 1 MiB random file in `/workspace` and a file in `/home/agent` kept their SHA-256 and content across a stop and start, and `e2fsck -fn` was clean afterwards.
- `pip` 25.1.1 runs from the image.

## Per-criterion results (equivalents for this helper)

| ID | Result | Evidence |
|---|---|---|
| A-1 | **Partial: macOS 15, a clean machine and notarization were not run.** Everything testable here passes. The helper runs as a child of a stand-in parent, ad hoc signed with only `com.apple.security.virtualization` (with or without hardened runtime). No Homebrew. No Docker at run time: the kernel, initramfs, root disk and workspace template are static files, and a workspace is an APFS clone. **Notarization and Gatekeeper: blocked, no Developer ID (D-26).** `spctl -a -vv -t exec` rejects both ad hoc builds. Not run on macOS 15 or on a clean machine; the binary targets `minos 15.0`, and the SDK headers mark every class and initialiser used as `API_AVAILABLE(macos(11.0))` or `(12.0)`. | `sign-*.txt`, `a1-signing.txt` |
| A-4 | **Pass with wide margin.** Start to first result 318–350 ms warm, 615–636 ms on a first launch after re-signing, against a 15 s target. Further command p50 about 0.6 ms, max 3.4 ms, against a 300 ms target. A cold page cache was not measurable without sudo. | `a4-timing.txt` |
| A-5 | **Pass on idle memory; return on stop passes; no return while running.** One idle VM costs about 165 MB footprint (helper 17 + VM 148), or 222 MB RSS. Two cost about 330 MB footprint. The helper alone is far under budget; the guest supervisor lives inside the VM's memory. Memory is returned on stop because the process exits. The balloon did not return memory while running (finding 4). | `a5-*.txt` |
| A-11 | **Pass.** (a) Parent SIGKILLed while `sleep 30` ran: the helper saw stdin EOF, unblocked the in-flight read, shut the guest down over a fresh vsock connection (`bye` received), and the VM stopped in 125 ms. Helper and VM process were gone **0.21 s** after the kill, and nothing was alive at +10 s. (b) Helper SIGKILLed while a command ran: VM process gone in **0.08 s**. The parent sees EOF, so the outcome is detectable and reported as unknown. (c) State is readable afterwards: files synced before each kill were present on the next boot. After (b), `e2fsck -fn` reported inconsistent counts ("Free inodes count wrong"); only the tail of its output was kept. After the next mount it was clean. (d) A second VM on the same workspace is impossible (finding 3). A 5 s watchdog bounds the helper's shutdown. | `a11-parent.txt`, `a11-helper.txt`, `a11-after.txt`, `a11-fsck-after-helper-kill.txt`, `lock-*.txt` |
| WP-0.5 items | **Pass**: boot the v0 root disk read-only, attach a read-write sparse workspace disk, exchange framed `hello` over vsock (generation and image digest echoed; generation mismatch closes the channel, `generation-mismatch.txt`), exec as uid 1000, stop. | `first-events.jsonl`, `guest-facts.txt` |

## How the kernel and boot configuration are supplied and updated

- **Spike.**
  - The kernel `Image`, the initramfs and the command line are plain files plus a string. The helper takes them as `--kernel`, `--initrd` and `--cmdline`, and adds `console=hvc0 loglevel=4 panic=-1 agentkit.gen=N agentkit.image=<digest>`.
  - Updating means replacing the files, which would be app resources, and restarting the VM.
  - The initramfs modules must match the kernel build exactly, so the kernel and initramfs are one versioned unit.
- **Production proposal.**
  - Build our own arm64 kernel config in CI with `virtio_blk`, vsock and balloon built in. That removes the modules and busybox.
  - Ship `agentkit-sandboxd` (static musl) as `/init` in a tiny initramfs, or on the root disk with `init=`.
  - Version the kernel together with the image manifest. GPL-2 means shipping or offering the kernel source.
  - Security updates to the kernel are then OneMind's job. That is the main ongoing cost of Track B on macOS.
- **Root disk, production.**
  - Build the ext4 (or erofs) root disk in Linux CI from the pinned OCI image (`mkfs.ext4 -d` or `mkfs.erofs`), digest-pinned and compressed, as a release asset beside the OCI archive.
  - The desktop downloads it once and verifies the digest. No container engine or e2fsprogs is needed on the Mac.
  - The `docker export` used here is spike-only.
- **Workspace disk, production.** An APFS clone of a shipped, pre-formatted sparse template. Growing it later needs `resize2fs` in the guest, which the v0 image does not have.

## Risks

- **Kernel ownership.** We pick, configure, patch and ship a Linux kernel and initramfs for macOS. Whether libkrun (libkrunfw) changes this was not examined here.
- **Memory is not returned while the VM runs.** The balloon showed no host reclaim. Long-lived VMs keep their peak guest usage until stop. Mitigations: stop idle VMs, use a smaller default memory size, or investigate other reclaim paths (not examined).
- **Platform coverage.**
  - Not run on macOS 15, on a clean machine, or with a Developer ID.
  - Notarization is blocked until a Developer ID exists.
  - The minimum OS is set by the APIs used, all macOS 12 or older per the SDK headers.
- **AppKit is linked** because of `objc2-virtualization` default features. Trimming features should lower the 27 MB helper RSS; not tried.
- **Guest supervisor.**
  - The responder is Python, spike-only. It serialises exec through PID 1 reaping.
  - `agentkit-sandboxd` still needs a musl cross-build in CI.
- **All FFI is `unsafe`.** Every VZ call must be made on the VM queue. The bindings are generated and the API is small, but every call site needs review.
- **Command-line metadata.** The kernel command line carries the generation and image digest. It is readable inside the guest and is not secret, but should not grow.
- **Host-initiated vsock connect.** The helper retries every 10 ms until the guest listens (10–15 attempts). A host-side listener (`VZVirtioSocketListener`) with the guest connecting would remove the polling but needs an Objective-C delegate class.

## Recommendation

Virtualization.framework works for the Track B macOS helper:
- every WP-0.5 item passes;
- A-4, A-5 (idle) and A-11 pass with large margins;
- the only entitlement is the unrestricted `com.apple.security.virtualization`;
- the VM's lifetime is tied to the helper process by the OS.

It adds no dependency beyond system frameworks and seven direct crates (22 in the lockfile).

The deciding factors against libkrun should be:
- the kernel and initramfs supply burden for each;
- the balloon and memory-return behaviour;
- binary size and licensing.

Pick by comparing the two reports. On these numbers alone, Virtualization.framework is a strong default because it needs no bundled hypervisor library and has first-party lifecycle and disk locking.

Open before a decision:
- a macOS 15 run;
- a Developer ID notarization run;
- a static Rust `agentkit-sandboxd` in place of the Python responder;
- a custom kernel config without modules.

## Reproduce

1. `scripts/mkdisks.sh`, run in `debian:bookworm-slim` as in the Setup section. Inputs are `disks/v0-rootfs.tar` from `docker export`, plus the kernel `.deb` extracted to `dl/`.
2. `scripts/mkinitrd.sh`, then copy `dl/x/root/boot/vmlinuz-*` to `build/Image`.
3. `scripts/build.sh`, which runs cargo build and signs `sign/vz-helper` with `sign/ent-virtualization.plist`.
4. `python3 scripts/driver.py disks/ws1.ext4 --bench 50 --cmd 'id'`, where `ws1.ext4` is `cp -c disks/workspace-template.ext4`.
5. Run the measurements with these scripts:
   - `scripts/timing.sh` (A-4);
   - `scripts/mem_test.py`, `scripts/balloon_test.py` (A-5);
   - `scripts/a11.sh parent|helper` (A-11).

## Cleanup

- Process check: `pgrep -lf 'vz-helper|driver.py|Virtualization.VirtualMachine'` shows only PID 69087, the pre-existing process.
- Deleted afterwards:
  - `helper/target/` (`cargo clean`: 184 files, 167.3 MiB);
  - every disk image, the sidecar locks and the 667 MB rootfs tar;
  - the kernel `.deb`, its extraction and the staged `build/Image`;
  - the busybox copy and the initramfs staging tree.
- Re-running needs steps 1–3 again.
- What remains in `$SPIKE` is 3.4 MB:
  - sources, scripts and logs;
  - the entitlement plists;
  - the signed `sign/vz-helper`;
  - `build/initrd.img`;
  - the decompressed modules in `dl/mods`.
