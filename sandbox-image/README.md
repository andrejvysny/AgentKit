# Agent sandbox image (v1)

The Linux image every agent sandbox runs: Debian stable slim plus the tools,
Python and Node an agent needs, a few fixed helpers the host drives over
exec (WP-0.2 of the Agent Sandbox spec; ADR 0038), and the guest
supervisor `agentkit-sandboxd` as its entrypoint (ADR 0039). One pinned
manifest builds the same OCI image, byte for byte, for `linux/amd64` and
`linux/arm64`.

## Files

| Path | What it is |
|---|---|
| `manifest.env` | The pins: base image digest, Debian snapshot, `SOURCE_DATE_EPOCH`, BuildKit image digest, Node version and tarball sha256s, PyPI cut-off, the Rust build image and the `agentkit-sandboxd` sha256 per platform |
| `apt-packages.txt` | Top-level Debian packages (versions come from the snapshot) |
| `requirements.in`, `requirements.lock` | Python libraries; the lock pins every wheel by version and sha256 |
| `Dockerfile` | The build; only `build.sh` passes it the right arguments |
| `build.sh` | Build the supervisor and the image, verify reproducibility, load, self-test, notice, re-lock |
| `out/agentkit-sandboxd-<arch>` | The supervisor binary `build.sh sandboxd` builds from `../rust` (not committed; pinned by sha256 in `manifest.env`) |
| `helpers/` | The fixed helpers installed in the image (below) |
| `rootfs/etc/sandbox/` | The fixed command environment and the image's own bash rc |
| `notice/generate-notice.py` | Writes the third-party notice inside the image at build time |
| `notice/agentkit-sandboxd-crates.txt` | The crates, Rust standard library and musl libc linked into `agentkit-sandboxd`, with licences (the notice's last section) |
| `notice/NOTICE-linux-<arch>.txt` | That notice, extracted from each built image |
| `lock/dpkg-linux-<arch>.txt` | Every Debian package and version that resolved, per architecture |
| `lock/image-digests.txt` | The digests two builds agreed on (written by `build.sh verify`) |

## Build and verify

Needs Docker with buildx and emulation of the other platform (OrbStack or
Docker Desktop on Apple silicon emulate amd64), network access to Docker
Hub, snapshot.debian.org, nodejs.org, PyPI and crates.io, and `python3` on
the host.

```sh
./build.sh sandboxd    # build out/agentkit-sandboxd-<arch>, print each sha256, compare with manifest.env
./build.sh verify      # build twice (2nd: fresh builder, --no-cache), compare, record digests
./build.sh load        # load each platform into docker as agentkit-sandbox:v1-<arch>
./build.sh selftest    # run sandbox-selftest on every platform
./build.sh notice      # refresh notice/ and lock/dpkg-* from the loaded images
./build.sh clean       # remove the dedicated builder, its cache and the cargo cache
```

`build.sh sandboxd` runs `cargo build --release --locked -p
agentkit-sandboxd` from `../rust` for each platform inside that platform's
variant of `RUST_IMAGE` (native on its own architecture, emulated
otherwise), named by its manifest digest read from the pinned index (the
index's sha256 is checked first): Docker's classic image store keeps one
platform per digest reference and refuses a second platform of the same
index ("cannot overwrite digest"). Crates are fetched into the
`agentkit-sandboxd-cargo` volume first, then the build runs with
`--network none --offline`, with the sources at `/src` and the target at
`/target` so the binary does not depend on the host's paths.
`RUSTUP_TOOLCHAIN` overrides `rust-toolchain.toml`, whose clippy and
rustfmt components the image lacks, so rustup never fetches anything. The
result is a static musl binary (static-pie on amd64; a static executable
on arm64, the aarch64 musl target's default), linked against the musl
libc Rust ships with the target; the Dockerfile refuses it unless its
sha256 equals `SANDBOXD_SHA256_<ARCH>`. On 2026-11-26 two builds from
clean target directories on the Mac (OrbStack: arm64 native, amd64 under
Rosetta) gave the same sha256 per platform; the amd64 sha256 differs from
the 2026-11-25 VM build because the sources changed in between, so builds
on different hosts have not been compared on one source tree.

`build.sh build [--no-cache] [OUT]` makes one OCI archive (default
`out/image.oci.tar`) and prints its digests: `index <digest>`, then per
platform the manifest digest and config digest. BuildKit writes a
one-platform build as a bare manifest; `build.sh finalize` wraps it in an
index (as a multi-platform build has), so the image is always referenced by
index digest, and names it `localhost/agentkit-sandbox:<version>` in
`index.json` (`io.containerd.image.name` and
`org.opencontainers.image.ref.name`), rewriting the archive in a fixed
order with fixed metadata; no blob changes. The config digest is what
`docker image inspect --format '{{.Id}}'` shows after `load`; `selftest`
refuses to run an image whose id differs from `lock/image-digests.txt`.

To check a build against the committed digests, run `./build.sh build`
and compare its output with `lock/image-digests.txt`.

How the build is made reproducible:

- Every input is pinned: base image and BuildKit by digest, apt by a
  snapshot.debian.org timestamp, Node by tarball sha256, wheels by sha256
  (`--require-hashes --only-binary=:all:`), `agentkit-sandboxd` by sha256
  (built from `rust/Cargo.lock` in the Rust image pinned by digest).
- A dedicated `docker-container` builder runs the pinned BuildKit; output is
  `type=oci,rewrite-timestamp=true` with `SOURCE_DATE_EPOCH` from the
  manifest, no provenance or SBOM attestations.
- Files whose content depends on build time are removed in the layer that
  creates them (apt lists and logs, ldconfig aux cache, `*-old`, caches),
  the agent user is written into `/etc/passwd` and friends directly, and all
  Python bytecode is recompiled as `unchecked-hash` .pyc in one process
  (`-j1`, `PYTHONHASHSEED=0`). marshal marks an object as shared when its
  refcount is above one, and a parallel worker's refcounts depend on which
  files it compiled before: with `-j0` a cold rebuild changed one arm64
  .pyc (2026-11-26).

Changing a pin: edit `manifest.env` (or `apt-packages.txt`,
`requirements.in` then `./build.sh lock-pip`), run `verify`, `load`,
`selftest`, `notice`, and commit the changed `lock/` and `notice/` files
with it. A change under `rust/` that reaches `agentkit-sandboxd` is a new
`SANDBOXD_SHA256_AMD64` and `SANDBOXD_SHA256_ARM64`: run `./build.sh
sandboxd` first, and when `Cargo.lock` changed what it links, regenerate
`notice/agentkit-sandboxd-crates.txt` (the command is in its header).

## What is in the image

- Debian trixie slim, upgraded to the snapshot. bash, coreutils, findutils,
  grep, sed, gawk, less, file, git, curl, ca-certificates, zip, unzip, tar,
  jq, ripgrep, tree; also tzdata (for `--tz`) and libatomic1 (needed by
  the Node.js binary).
- Python 3.13 (Debian) with pip and venv; pandas, numpy, matplotlib,
  openpyxl, requests and their dependencies from PyPI in
  `/usr/local/lib/python3.13/dist-packages`.
- Node.js 26 LTS with its bundled npm in `/usr/local`.
- User `agent`, uid and gid 1000, home `/home/agent`, default working
  directory `/workspace`. No sudo; every setuid/setgid bit is stripped and
  the build fails if one remains.
- User-level installs land in the home: `pip install` falls back to a user
  install in `~/.local` (the system site is not writable and the
  `EXTERNALLY-MANAGED` marker is removed), npm's builtin config sets the
  global prefix to `~/.npm-global`. Both bin directories are last on PATH.
  Emptying `/home/agent` resets them.
- `/usr/share/doc/sandbox-image/NOTICE` (third-party notice: Debian
  packages, Python distributions, Node.js and npm, and the crates, Rust
  standard library and musl libc linked into `agentkit-sandboxd`) and
  `requirements.lock`.
- `/usr/local/bin/agentkit-sandboxd` (root, 0755), the entrypoint: the guest
  supervisor of a server sandbox (ADR 0039). It is PID 1 of the container,
  dials the host's control socket at `/run/agentkit`, runs commands and file
  operations for the host, and serves the egress shim on
  `127.0.0.1:3128`. The image has no `CMD`; run a helper by overriding the
  entrypoint (`--entrypoint sandbox-selftest`).
- Proxy settings for that shim: `/etc/sandbox/environment` sets
  `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy` to
  `http://127.0.0.1:3128` and `NO_PROXY`/`no_proxy` to
  `localhost,127.0.0.1,::1`; curl and pip (requests) read them. npm's
  builtin config (`/usr/local/lib/node_modules/npm/npmrc`) also sets
  `proxy` and `https-proxy`, so npm keeps the shim when a program clears
  the environment. The container has no other way out: a program that
  ignores the proxy fails closed.

## Helpers

All are Python scripts run with `python3 -I` (isolated: no PYTHONPATH, no
user site), standard library only, in `/usr/local/bin` with their shared
module in `/usr/local/lib/sandbox`.

### sandbox-exec

```
sandbox-exec --marker OP [--deadline-ms N] [--output-limit BYTES]
             [--cwd DIR] [--tz ZONE] [--grace-ms N] -- 'COMMAND'
```

- Runs `bash --noprofile --norc -c COMMAND` from an empty environment plus
  `/etc/sandbox/environment`; `BASH_ENV` points at `/etc/sandbox/bashrc` on
  the read-only image, so nothing in `/home/agent` is sourced. stdin is
  `/dev/null`.
- New session and process group. `SANDBOX_OP_MARKER=OP` is in the
  environment of everything the command starts. `/tmp/sandbox-ops/OP.json`
  holds the group while running and the final status after.
- Deadline (default 600 s): SIGTERM to the tree, SIGKILL after the grace
  period (default 2 s), exit code 124.
- Output cap (default 1 MiB, stdout and stderr together): output past the
  cap is read and discarded, so the command never blocks on a full pipe.
- When the shell exits, whatever it left running in the tree is ended.
- The last stderr line is always `SANDBOX_EXEC_STATUS {json}` with
  `exit_code`, `signal`, `timed_out`, `output_bytes`, `forwarded_bytes`,
  `truncated`, `remaining_processes`, `duration_ms`. The exit code is the
  command's, 128+N for a signal, 124 for the deadline.
- SIGTERM, SIGHUP or SIGINT to the wrapper ends the tree too.

### sandbox-kill

`sandbox-kill [--grace-ms N] OP` ends every process whose environment holds
exactly `SANDBOX_OP_MARKER=OP`, plus the op's process group while it runs:
SIGTERM, then SIGKILL until nothing matches. This is the "kill by marker"
the host runs after cancelling an exec stream, and it works when the
wrapper is gone. A process that both leaves the group and clears its
environment escapes it; the marker is a convention, not a kernel boundary.

### sandbox-fs

```
sandbox-fs write --path P --sha256 HEX --size N [--mode 0644] [--cwd DIR] < data
sandbox-fs read  --path P [--offset N] [--limit N] [--cwd DIR]            > data
sandbox-fs list  --path P [--depth N] [--limit N] [--cwd DIR]             > JSON lines
```

- Paths must be under `/workspace` or `/home/agent`; relative paths are
  taken against `--cwd`.
- Resolution is descriptor-based: one component at a time from an open root
  with `O_NOFOLLOW`. `..` above a root is refused. A symlink is followed only
  if its target stays inside a root (relative targets from the link's
  directory, absolute targets must start with one of the roots); otherwise
  `symlink_escape`. At most 40 links.
- `write` streams stdin into a temp file in the target directory, checks
  size and SHA-256, fsyncs and renames over the target. A mismatch removes
  the temp file and leaves the target as it was. A symlink at the final name
  is replaced, never written through. Missing parent directories are
  created.
- `read` serves regular files only; bytes on stdout, then
  `{"size","offset","returned","eof"}` on stderr.
- `list` prints entries depth-first in name order
  (`path,type,size,mode,mtime[,target]`), never descends into symlinks, and
  ends with `{"summary":{"count","truncated"}}`.
- Errors are one JSON line on stderr (`{"error","message"}`), exit 2 for a
  refusal and 1 for an I/O error.

### sandbox-selftest

Checks identity (uid/gid 1000, no extra groups), no sudo, no
setuid/setgid/file capabilities, nothing owned by the agent or
world-writable outside the roots, read-only root, the tool list, the Python
libraries against the shipped lock (and a real pandas to Excel to
matplotlib round trip), venv, user-level pip and npm installs, and every
helper behaviour above: clean environment and image rc, deadline with a
`setsid` escapee, SIGKILL escalation, output cap and drain, background job,
kill by marker, wrapper SIGTERM, hash and size mismatch, symlink, `..` and
outside-root refusals, in-root links, offset/limit and list depth/limit.
`build.sh selftest` runs it as
`docker run --init --network none --read-only --tmpfs /tmp --user 1000:1000 --cap-drop ALL --security-opt no-new-privileges`
with fresh volumes on `/workspace` and `/home/agent`.

## Measured (v1)

On 2026-11-26, macOS arm64, OrbStack (Docker 29.8.2), BuildKit v0.35.1,
amd64 under Rosetta emulation:

| | linux/amd64 | linux/arm64 |
|---|---|---|
| Unpacked root filesystem (`du -sxb /`) | 653 MB | 686 MB |
| Compressed layers (15) | 226 MB | 226 MB |
| Debian packages | 149 | 149 |
| `agentkit-sandboxd` | 1 356 576 bytes, sha256 `cdcc41ac…` | 1 249 552 bytes, sha256 `b0a07a6d…` |
| `sandbox-selftest` | 21 of 21 pass | 21 of 21 pass |

Digests (also in `lock/image-digests.txt`; the full sandboxd sha256s are
in `manifest.env`):

| | Digest |
|---|---|
| index (the image reference) | `sha256:78e813b333317aef6ad8bdc4cf3682b9752b05f54f88eb8051535dd27a07de93` |
| linux/amd64 manifest | `sha256:c0eec9f411447d46d6b38ffba549464234a31679f6a21cabe5ca0f8752973959` |
| linux/amd64 config (Docker classic store and Podman image id) | `sha256:687fe67511222c7fbb32c7f7230a929cd77038c88ab82169c3501841eae935d9` |
| linux/arm64 manifest | `sha256:6fcb57337145e46cf39809c40041793093920ca1e729bc89a958531518dbfa16` |
| linux/arm64 config | `sha256:2a4258daebe0a62554ef7d23f12089cd4e20e0edfb0dfe61504006a29a308f32` |
| `out/image.oci.tar` | sha256 `f6f21eb6ad3c2735ea747a2bc00fbf8339aac6fc33b6dab8d54cd3b7e66d9a7f`, 452 567 040 bytes |

`./build.sh verify`: the warm build and a cold build (fresh builder,
`--no-cache`) gave the same index, manifest and config digests, and the two
OCI archives were byte-identical; a third cold build (`build --no-cache`)
gave the same archive. The first `verify` (with `compileall -j0`) differed
in one arm64 .pyc (see "How the build is made reproducible").

Reproducibility limits: all builds ran on one host. Files from the build
context get the epoch as mtime only because their own mtime is later than
it (rewrite-timestamp clamps, it does not raise). A different BuildKit
version can compress differently, which is why it is pinned.

v0 (2026-11-25, same host and BuildKit, 13 layers): 652 MB amd64 and
685 MB arm64 unpacked, 226 and 225 MB compressed, index
`sha256:98473624…`.

## Installing on a server

The launcher references the image as
`localhost/agentkit-sandbox@<index digest>` (ADR 0039), the one form
Docker 29 (containerd image store) and rootless Podman 4.9/5.4 both accept.
`docker load -i out/image.oci.tar` (rootful) and `podman load -i
out/image.oci.tar` (as the rootless user) both name it
`localhost/agentkit-sandbox:<version>` from `index.json`, so no tag is
needed; the launcher's `image_digest` is the `index` line `build.sh` prints.
Docker then shows the index digest as the image id, Podman the config
digest (`docs/evidence/sandbox-m2/wp-2.1-conformance.md`).

## v1

- New: `agentkit-sandboxd` as entrypoint (no `CMD`), the proxy variables in
  `/etc/sandbox/environment`, npm's `proxy`/`https-proxy`, `build.sh
  sandboxd` and `finalize`, `--entrypoint` in `selftest` and `notice`.
- Both platforms: `agentkit-sandboxd` is built per platform in the pinned
  Rust image's own variant (`SANDBOXD_SHA256_AMD64`, `SANDBOXD_SHA256_ARM64`).
- The notice lists the crates, Rust standard library and musl libc linked
  into `agentkit-sandboxd` (`notice/agentkit-sandboxd-crates.txt`).
- Built from scratch with `verify`, `load`, `selftest` and `notice` on
  2026-11-26 (above). The WP-2.1 real-runtime qualification of this index
  digest on Docker 29 and rootless Podman 4.9/5.4 (amd64) is in
  `docs/evidence/sandbox-m2/wp-2.1-conformance.md`, "After the security
  fixes". arm64 passed `sandbox-selftest` only; no arm64 runtime host was
  qualified.

## Not covered yet

- The VM backend's root disk, kernel and boot configuration (the manifest
  is ready for it; WP-0.3 and later).
- Full command logs in `/workspace/.sandbox/logs`: the wrapper caps the
  stream; writing the full log is left to the backend adapter.
- Publishing: GitHub release assets and a registry push.
- arm64 on a real launcher and runtime (only `sandbox-selftest` ran).
- The NOTICE names the licences of the crates linked into
  `agentkit-sandboxd` but does not carry their full texts.
