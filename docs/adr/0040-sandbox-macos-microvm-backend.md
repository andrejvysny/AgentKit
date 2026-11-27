# ADR 0040: Track B macOS microVM backend (Milestone 3)

Status: accepted for implementation, 2026-11-27. Builds on ADR 0038
(workspaces, "Choice: Virtualization.framework, provisional") and ADR 0039
(server backend). The Virtualization.framework choice stays provisional
until the re-check ADR 0038 names: a clean macOS 15 machine, a Developer ID
build and the real musl `agentkit-sandboxd` as the guest supervisor.

## Context

The desktop app must run the same sandbox scenario as the server, with no
Docker installed (spec Milestone 3, SBX-01, SBX-13, SBX-15). Milestone 2
shipped `agentkit-sandbox-container` (the `SandboxBackend` port over a
launcher), `agentkit-sandbox-launcher` (a fixed-template launcher service),
`agentkit-sandboxd` (guest supervisor), `agentkit-egress` and
`agentkit-sandbox-proto`. On a Mac the isolation boundary is a microVM
(Virtualization.framework), one helper process per running VM.

## Decision

### 1. Reuse the container adapter; replace only the launcher

`agentkit-sandbox-microvm` (layer 4) provides `VmLauncher`: an in-process
service that answers the existing launcher protocol
(`agentkit-sandbox-proto::launcher`: `ensure`, `stop`, `inspect`, `usage`,
`reset_home`, `destroy`) on a unix socket, and starts one helper process
per running machine instead of a container. The desktop composes the
unchanged `ContainerBackend` with `launcher_socket` pointing at the
`VmLauncher` socket and `socket_dir` at the `VmLauncher` socket root. The
guest channel, egress proxy, generation rules, registry and mapping are the
server's code. The only change allowed in `agentkit-sandbox-container` is a
config field that sets the `describe()` kind and hard-quota answer for a VM
(the workspace disk size is the quota).

The `VmLauncher` keeps the launcher's rules: a generation never runs twice;
`ensure` refuses a generation at or below one proven stopped or below one it
started; `stop` answers `proven` only when the helper process of that
generation has exited and been reaped and no helper record of the workspace
at or below it is live; at most `max_machines` run at once; state (started
and proven-stopped generations per workspace, live helper pids) persists in
its state directory, so the rules hold across app restarts. On startup it
reads its records, kills (SIGKILL) and reaps any helper pid that is still
ours (checked by pid plus process start time, never by pid alone), then
treats those generations as proven stopped.

### 2. Disks and app-data layout

Per workspace, under the `VmLauncher` data root (mode 0700):

```
<data_root>/workspaces/<machine name of the workspace>/workspace.img   raw sparse, ext4 made by the guest
<data_root>/workspaces/<machine name of the workspace>/home.img        raw sparse, ext4 made by the guest
<data_root>/state/…                                                    generation records, helper records
```

- Sizes come from config: workspace disk (default 20 GiB), home disk
  (default 4 GiB). Files are created sparse (`set_len`); `usage` answers
  allocated bytes (`st_blocks * 512`) of both.
- `reset_home` deletes and recreates `home.img`; `destroy` deletes both.
  Both are refused while a machine of the workspace runs.
- The image files (kernel and root disk) are read-only inputs named in the
  config by path and sha256; the `VmLauncher` verifies their sha256 at
  startup and refuses to start a machine with an image that does not match.

Socket root: a short per-user directory, mode 0700, owned by the user
(`$TMPDIR`-based on macOS, for example `/var/folders/…/T/aksbx/`), because
`sun_path` is 104 bytes on macOS. Every socket path is checked against that
limit at config validation with the fixed-length machine directory name
(`machine_dir_name`, 24 bytes).

### 3. Helper process (`agentkit-sandbox-vmhelper`)

A macOS-only binary in the microvm crate, signed with exactly one
entitlement, `com.apple.security.virtualization` (no hypervisor key, no
other app entitlement). One process per running VM.

- Input: one JSON object on its first stdin line, `deny_unknown_fields`:

```json
{
  "protocol": 1,
  "kernel": "/abs/path/Image",
  "root_disk": "/abs/path/root.ext4",
  "workspace_disk": "/abs/path/workspace.img",
  "home_disk": "/abs/path/home.img",
  "cpus": 2,
  "memory_bytes": 2147483648,
  "cmdline": "console=hvc0 …",
  "control_socket": "/abs/path/<machine dir>/control.sock",
  "egress_socket": "/abs/path/<machine dir>/egress.sock",
  "console_log": "/abs/path/console.log"
}
```

- Output: JSON lines on stdout: `{"event":"started"}` after the VM starts,
  `{"event":"connected"}` after the control bridge is up,
  `{"event":"stopped","reason":"…"}` last. Diagnostics go to stderr.
- Disks: vda = root disk, read-only; vdb = workspace disk, read-write;
  vdc = home disk, read-write; cached mode as in the spike. No network
  device. One virtio-socket device, one entropy device, one balloon.
- Control bridge (host initiated): after start the helper connects to guest
  vsock port **1024** (retrying until a deadline of 30 s), then connects to
  `control_socket` and splices the two streams until either side closes.
  Exactly one control connection per VM lifetime; if it ends, the helper
  stops the VM. Guest processes never reach the host control socket.
- Egress bridge (guest initiated): the helper listens on vsock port
  **1025**; every guest connection is spliced to a new connection to
  `egress_socket`. Any guest process may use it; the egress proxy enforces
  the policy, exactly as the container's shim does.
- Lifetime: the VM never outlives the helper. EOF on stdin (parent gone)
  starts a stop: request the guest to stop, after 3 s force stop, then exit.
  SIGTERM does the same. The helper sets its own parent-death check (poll
  `getppid() == 1`) as a second guard.
- Exit status 0 only when the VM reached the stopped state.

### 4. Guest boot contract

The VM root disk is an ext4 image built from the same `agentkit-sandbox`
OCI image (arm64) plus a VM layer: `e2fsprogs` and
`/usr/local/sbin/agentkit-vm-init`. No initramfs; the kernel has
`virtio_blk`, `ext4`, `vsock` with the virtio transport, virtio balloon,
virtio entropy and `hvc` console built in.

Kernel command line (the `VmLauncher` composes it; keys are fixed):

```
console=hvc0 root=/dev/vda rootfstype=ext4 ro init=/usr/local/sbin/agentkit-vm-init panic=-1 quiet
agentkit.generation=<u64> agentkit.image=<sha256 of root disk> agentkit.token=<channel token>
agentkit.control_port=1024 agentkit.egress_port=1025
```

`agentkit-vm-init` (PID 1, shell):
1. Mounts `proc`, `sys`, `devtmpfs`; reads the `agentkit.*` keys, then
   bind-mounts an empty root-only file over `/proc/cmdline` so uid 1000
   cannot read the token.
2. Formats vdb and vdc with `mkfs.ext4` when they carry no ext4 signature;
   mounts vdb at `/workspace` and vdc at `/home/agent` (owner 1000:1000);
   tmpfs at `/tmp` and `/run` as the container template does.
3. Applies the image's `/etc/sandbox/environment` and starts
   `agentkit-sandboxd` as root with `AGENTKIT_CONTROL_VSOCK_PORT=1024`,
   `AGENTKIT_EGRESS_VSOCK_PORT=1025`, `AGENTKIT_GENERATION`,
   `AGENTKIT_IMAGE_DIGEST`, `AGENTKIT_CHANNEL_TOKEN`; powers off when it
   exits.

`agentkit-sandboxd` gains a vsock transport (Linux only): with
`AGENTKIT_CONTROL_VSOCK_PORT` set it **listens** on that vsock port (bound
before any user process exists, so no other process can take it), accepts
exactly one connection and then runs the same session as over the unix
socket. With `AGENTKIT_EGRESS_VSOCK_PORT` set, its egress shim
(`127.0.0.1:3128`) connects to the host CID (2) on that port instead of
the egress unix socket. Unix socket mode is unchanged.

### 5. Image artifacts and manifest

`sandbox-image/vm/` builds, inside Docker (Linux arm64, pinned inputs):
`Image` (arm64 kernel, own config, no modules), `root.ext4` and
`vm-manifest.json`:

```json
{
  "schema": 1,
  "name": "agentkit-sandbox-vm",
  "version": "v1",
  "arch": "arm64",
  "ociImageDigest": "sha256:…",
  "files": [
    {"role": "kernel", "path": "Image", "size": 0, "sha256": "…"},
    {"role": "root", "path": "root.ext4", "size": 0, "sha256": "…"}
  ],
  "kernel": {"version": "6.12.x", "configSha256": "…"},
  "licenses": "NOTICE"
}
```

A consumer pins the sha256 of `vm-manifest.json`; the manifest pins every
file. The kernel's GPL-2.0 source obligation is met by recording the exact
upstream tarball and config.

### 6. Conformance

The `agentkit-testkit` backend conformance suite runs against
`ContainerBackend` over `VmLauncher` with real helpers, as an `#[ignore]`d
macOS test gated on `AGENTKIT_MICROVM_HELPER`, `AGENTKIT_MICROVM_KERNEL`,
`AGENTKIT_MICROVM_ROOT` (the helper must be ad hoc signed with the
entitlement first; `scripts/sign-helper.sh`). Unit tests of `VmLauncher`
use a fake helper and run everywhere.

## Consequences

- Two processes per VM (helper plus Apple's VM service); SBX-13 tests must
  show both gone.
- Memory the guest frees is returned only on stop (ADR 0038).
- The host never connects into a guest port other than 1024, and only the
  helper does; guest uid 1000 can reach only the egress port.
- Not decided here: a Developer ID signing and notarization flow (blocked
  on the owner's Apple Developer enrolment), libkrun as fallback.
