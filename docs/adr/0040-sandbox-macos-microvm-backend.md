# ADR 0040: Track B macOS microVM backend (Milestone 3)

Status: accepted for implementation, 2026-11-27. Builds on ADR 0038
(workspaces, "Choice: Virtualization.framework, provisional") and ADR 0039
(server backend). The Virtualization.framework choice stays provisional
until the re-check ADR 0038 names: a clean macOS 15 machine, a Developer ID
build and the real musl `agentkit-sandboxd` as the guest supervisor.
Amended 2026-11-28 after the review of the first implementation: stop
proofs need released disks, the console is capped, egress is bounded, the
guest supervisor never runs as root, and sections 3 and 4 describe what
was built.

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
(the workspace and home disk sizes together are the quota: storage usage
counts both trees, so the quota covers both disks).

The `VmLauncher` keeps the launcher's rules: a generation never runs twice;
`ensure` refuses a generation at or below one proven stopped or below one it
started; `stop` answers `proven` only when the helper process of that
generation has exited and been reaped, no helper record of the workspace
at or below it is live, and no process has the workspace's `workspace.img`
or `home.img` open any more; at most `max_machines` run at once; state
(started and proven-stopped generations per workspace, live helper pids)
persists in its state directory, so the rules hold across app restarts. On
startup it reads its records, kills (SIGKILL) and reaps any helper pid that
is still ours (checked by pid plus process start time, never by pid
alone), waits for the workspace's disks to be released, then treats those
generations as proven stopped; a helper or a disk holder that is not gone
stops the launcher from starting (fail closed).

A helper's exit is not proof on its own, whatever its status: the VM runs
in Apple's VM service process, which may outlive a killed helper for a
moment. So a stop (and startup recovery) lists the processes with a
descriptor on either disk (every process's descriptors, read with
`proc_pidinfo` through `agentkit-sys`; `proc_listpidspath` was measured to
miss a holder that opens and closes other files meanwhile) and polls for
up to 10 s. Only this user's processes are visible, and descriptors only
(no working directories or mappings), so the check relies on the VM
service running as the user and using descriptors; that is still to be
confirmed on a real VM (the re-check above). Disks still held answer `proven: false` with a
reason and keep the machine held, so a later stop retries; a stop below a
held machine of a higher generation skips the check, since that machine
started only after the disks were released and holds them itself. `ensure` does
the same check before it starts a helper and refuses a start
(`runtime_failed`, nothing recorded) while the disks are held, so two VMs
never share a disk.

Shutdown (`VmLauncher::shutdown`) first refuses every request
(`runtime_failed`), then stops every held machine at once; a start still
running when the shutdown began stops its helper instead of holding it.

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
- Restriction: only the data root and the socket root themselves are
  checked (a directory, not a link, owned by the user, mode 0700), not
  their ancestors, because macOS's per-user `$TMPDIR` lies below the `/var`
  link. Every ancestor must be writable by the user and root only, as the
  user's `Library` and `$TMPDIR` are; the configuration documents this.
- `reset_home` deletes and recreates `home.img`; `destroy` deletes both.
  Both are refused while a machine of the workspace runs.
- The image files (kernel and root disk) are read-only inputs named in the
  config by path and sha256; the `VmLauncher` verifies their sha256 at
  startup and refuses to start a machine with an image that does not match.
  Before each start the files' device, inode, size, modification time and
  change time must still be the ones hashed; the change time cannot be set
  back by a writer, so any change, a metadata-only one included, refuses
  starts until the launcher restarts and hashes again.

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
  `{"event":"connected"}` once the helper holds its one connection to guest
  port 1024 and before it connects to `control_socket` (the adapter binds
  that socket only after `ensured`, which the launcher answers on this
  event, so the helper then retries its connect for up to 30 s),
  `{"event":"stopped","reason":"…"}` last. Diagnostics go to stderr, which
  the launcher writes to the machine's `helper.log`; egress diagnostics are
  at most one line per 10 s, the next one counting the lines held back.
- Disks: vda = root disk, read-only, caching mode `cached` and
  synchronization `none` (shared read-only by every VM); vdb = workspace
  disk and vdc = home disk, read-write, caching mode `automatic` and
  synchronization `full` (each guest flush reaches the host's disk, so an
  acknowledged write survives a host crash). No network device. One
  virtio-socket device, one entropy device, one balloon.
- Console: `hvc0` goes into a pipe that the helper drains into
  `console_log` (created 0600, truncated at start). The file keeps the
  first 1 MiB and one line noting the cut; the rest is read and dropped, so
  the file stays bounded and the guest never blocks on its console. At a
  stop the helper waits up to 500 ms for the console to go quiet.
- Control bridge (host initiated): after start the helper connects to guest
  vsock port **1024**, one attempt at a time (the next only after the
  framework answered the previous), retrying until a deadline of 30 s,
  then connects to `control_socket` and splices the two streams until
  either side closes. Exactly one control connection per VM lifetime: the
  helper never re-dials, and if it ends, the helper stops the VM. Guest
  processes never reach the host control socket.
- Egress bridge (guest initiated): the helper listens on vsock port
  **1025**; every guest connection is spliced to a new connection to
  `egress_socket`. Any guest process may use it; the egress proxy enforces
  the policy, exactly as the container's shim does. At most 64 bridges run
  at once; one more is refused where the framework offers it. The guest's
  end of stream is passed on as a half close; the proxy's end of stream
  ends the bridge; a bridge no byte crossed for 10 minutes is shut down.
  The helper raises its open-file soft limit to the hard limit or 4096,
  whichever is lower.
- Lifetime: the VM never outlives the helper. EOF on stdin (parent gone)
  starts a stop: request the guest to stop, after 3 s force stop, then exit.
  SIGTERM does the same. The helper sets its own parent-death check (poll
  `getppid() == 1`) as a second guard.
- Exit status 0 only when the VM reached the stopped state; 1 when it
  ended in another state; 2 when the configuration or the start was
  refused; 3 when the stop outran its 15 s watchdog. The launcher keeps the
  status and logs a non-zero one, but no status proves a stop (section 1).

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
3. Sets `kernel.dmesg_restrict=1` (the kernel log holds the command
   line), applies the image's `/etc/sandbox/environment` and starts
   `agentkit-sandboxd` as uid 1000, gid 1000 with the supplementary groups
   cleared (`setpriv --reuid=1000 --regid=1000 --clear-groups`), never as
   root, so no workload command runs as root, with
   `AGENTKIT_CONTROL_VSOCK_PORT=1024`, `AGENTKIT_EGRESS_VSOCK_PORT=1025`,
   `AGENTKIT_GENERATION`, `AGENTKIT_IMAGE_DIGEST`,
   `AGENTKIT_CHANNEL_TOKEN`; powers off when it exits. (The init belongs
   to the image build; this is its contract.)

`agentkit-sandboxd` gains a vsock transport (Linux only): with
`AGENTKIT_CONTROL_VSOCK_PORT` set it **listens** on that vsock port (bound
before any user process exists, so no other process can take it), accepts
exactly one connection whose peer is the host (CID 2; a connection from
any other CID is refused and the wait goes on), closes the listener and
then runs the same session as over the unix socket. With
`AGENTKIT_EGRESS_VSOCK_PORT` set, its egress shim (`127.0.0.1:3128`)
connects to the host CID (2) on that port instead of the egress unix
socket. Unix socket mode is unchanged. As defence in depth, a supervisor
with either vsock variable set refuses to start (exit 2) when its
effective uid is 0.

The token's residual exposure: it reaches the supervisor in its
environment, and the supervisor runs as uid 1000 like the workload. Its
`/proc/<pid>/environ` is still closed to the workload, because the
supervisor makes itself non-dumpable (`PR_SET_DUMPABLE 0`), which gives its
`/proc` entries to root. And a token a guest process did learn is
harmless: the control connection is host-initiated, made once per machine
by the helper and never re-dialed, and the supervisor's listener is
closed after it, so a guest process has no channel to present it on.

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
  helper does, once per machine; guest uid 1000 can reach only the egress
  port.
- Not decided here: a Developer ID signing and notarization flow (blocked
  on the owner's Apple Developer enrolment), libkrun as fallback.
