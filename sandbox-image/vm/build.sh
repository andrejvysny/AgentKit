#!/usr/bin/env bash
# Build the agent sandbox microVM artifacts (ADR 0040 §4-§5) into vm/out/.
#
#   vm/build.sh kernel      out/Image and out/kernel.config (upstream tarball + agentkit.config)
#   vm/build.sh root        out/root.ext4 from ../out/image.oci.tar plus the VM layer (build-root.sh)
#   vm/build.sh manifest    out/vm-manifest.json and out/NOTICE from the files in out/
#   vm/build.sh all         kernel, root, manifest
#   vm/build.sh verify      build root.ext4 twice (second: --no-cache) and compare the sha256
#   vm/build.sh smoke       static checks in a Linux container (file, e2fsck -fn, modes, kernel config)
#   vm/build.sh clean       remove out/work and the kernel and root build cache of the builder
set -euo pipefail

VM="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE_DIR="$(cd "$VM/.." && pwd)"
cd "$VM"
# shellcheck source=../manifest.env
source "$IMAGE_DIR/manifest.env"
# shellcheck source=manifest.env
source "$VM/manifest.env"
export SOURCE_DATE_EPOCH
BUILDER=agentkit-sandbox-builder
OUT="$VM/out"
mkdir -p "$OUT"

log() { printf '==> %s\n' "$*" >&2; }

sha256_of() { python3 -I -c 'import hashlib, sys
h = hashlib.sha256()
with open(sys.argv[1], "rb") as f:
    for chunk in iter(lambda: f.read(1 << 20), b""):
        h.update(chunk)
print(h.hexdigest())' "$1"; }

size_of() { python3 -I -c 'import os, sys; print(os.path.getsize(sys.argv[1]))' "$1"; }

# The same dedicated builder (pinned BuildKit) as ../build.sh.
ensure_builder() {
  if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
    log "creating buildx builder $BUILDER ($BUILDKIT_IMAGE)"
    docker buildx create --name "$BUILDER" --driver docker-container \
      --driver-opt "image=$BUILDKIT_IMAGE" --bootstrap >/dev/null
  fi
}

cmd_kernel() {
  local cache=() stage="$OUT/work/kernel"
  if [[ "${1:-}" == "--no-cache" ]]; then cache=(--no-cache); fi
  ensure_builder
  rm -rf "$stage"; mkdir -p "$stage"
  log "building linux $KERNEL_VERSION (arm64, allnoconfig + kernel/agentkit.config)"
  docker buildx build --builder "$BUILDER" --platform linux/arm64 \
    --provenance=false --sbom=false ${cache[@]+"${cache[@]}"} \
    --build-arg "BASE_IMAGE=$BASE_IMAGE" \
    --build-arg "DEBIAN_SNAPSHOT=$DEBIAN_SNAPSHOT" \
    --build-arg "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH" \
    --build-arg "KERNEL_VERSION=$KERNEL_VERSION" \
    --build-arg "KERNEL_URL=$KERNEL_URL" \
    --build-arg "KERNEL_SHA256=$KERNEL_SHA256" \
    --output "type=local,dest=$stage" \
    -f "$VM/kernel/Dockerfile" "$VM"
  mv "$stage/Image" "$OUT/Image"
  mv "$stage/kernel.config" "$OUT/kernel.config"
  echo "Image $(sha256_of "$OUT/Image") $(size_of "$OUT/Image")"
  echo "kernel.config $(sha256_of "$OUT/kernel.config")"
}

cmd_root() {
  "$VM/build-root.sh" "$@"
  echo "root.ext4 $(sha256_of "$OUT/root.ext4") $(size_of "$OUT/root.ext4")"
}

need_outputs() {
  local f
  for f in Image kernel.config root.ext4 root-oci-digest.txt vm-packages.txt; do
    [[ -f "$OUT/$f" ]] || { log "out/$f is missing: run vm/build.sh kernel and root first"; return 1; }
  done
}

# vm-manifest.json in the shape of ADR 0040 §5; a consumer pins its sha256.
cmd_manifest() {
  need_outputs
  python3 -I - "$OUT" "$VM_NAME" "$VM_VERSION" "$VM_ARCH" "$KERNEL_VERSION" <<'EOF' > "$OUT/vm-manifest.json"
import hashlib, json, os, sys
out, name, version, arch, kernel_version = sys.argv[1:]
def sha256(path):
    h = hashlib.sha256()
    with open(os.path.join(out, path), "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()
entry = lambda role, path: {"role": role, "path": path,
                            "size": os.path.getsize(os.path.join(out, path)), "sha256": sha256(path)}
manifest = {
    "schema": 1,
    "name": name,
    "version": version,
    "arch": arch,
    "ociImageDigest": open(os.path.join(out, "root-oci-digest.txt")).read().strip(),
    "files": [entry("kernel", "Image"), entry("root", "root.ext4")],
    "kernel": {"version": kernel_version, "configSha256": sha256("kernel.config")},
    "licenses": "NOTICE",
}
print(json.dumps(manifest, indent=2))
EOF
  write_notice > "$OUT/NOTICE"
  echo "vm-manifest.json $(sha256_of "$OUT/vm-manifest.json")"
  cat "$OUT/vm-manifest.json"
}

write_notice() {
  cat <<EOF
$VM_NAME $VM_VERSION ($VM_ARCH): third-party notice

Image: the Linux kernel $KERNEL_VERSION, GPL-2.0 WITH Linux-syscall-note.
  Unmodified upstream source, no patches:
    $KERNEL_URL
    sha256 $KERNEL_SHA256
  Configuration: kernel.config (sha256 $(sha256_of "$OUT/kernel.config")),
  made by "make allnoconfig" plus sandbox-image/vm/kernel/agentkit.config and
  "make olddefconfig". No modules. The tarball and kernel.config are the
  complete corresponding source; they are shipped or offered with the Image.

root.ext4: the agent sandbox OCI image $(cat "$OUT/root-oci-digest.txt")
  (linux/arm64). Its own notice (Debian packages, Python distributions,
  Node.js and npm, the crates linked into agentkit-sandboxd) is
  /usr/share/doc/sandbox-image/NOTICE on the disk. The VM layer adds these
  Debian packages (snapshot $DEBIAN_SNAPSHOT); each one's licence is
  /usr/share/doc/<package>/copyright on the disk:
EOF
  sed 's/^/    /' "$OUT/vm-packages.txt"
  cat <<EOF
  and /usr/local/sbin/agentkit-vm-init, part of AgentKit.
EOF
}

# Two root builds, the second without the build cache (fresh unpack, apt and
# mkfs), must give the same bytes.
cmd_verify() {
  local a="$OUT/work/verify-1" b="$OUT/work/verify-2" sa sb
  "$VM/build-root.sh" "$a"
  "$VM/build-root.sh" --no-cache "$b"
  sa="$(sha256_of "$a/root.ext4")"; sb="$(sha256_of "$b/root.ext4")"
  echo "build 1 root.ext4 $sa"
  echo "build 2 root.ext4 $sb"
  [[ "$sa" == "$sb" ]] || { log "ROOT DISKS DIFFER"; return 1; }
  log "reproducible: both builds gave $sa"
}

# Static checks without a VM, in a Linux container with e2fsprogs and file.
cmd_smoke() {
  need_outputs
  ensure_builder
  local tools=agentkit-sandbox-vm-tools:"$VM_VERSION"
  docker buildx build --builder "$BUILDER" --platform linux/arm64 --load -q \
    --build-arg "BASE_IMAGE=$BASE_IMAGE" --build-arg "DEBIAN_SNAPSHOT=$DEBIAN_SNAPSHOT" \
    --target tools -t "$tools" -f "$VM/root/Dockerfile" "$VM" >/dev/null
  docker run --rm --network none --platform linux/arm64 \
    -v "$OUT:/vm:ro" -v "$VM/kernel:/kernel:ro" -v "$VM/rootfs:/src:ro" "$tools" \
    bash -euo pipefail -c "$(smoke_script)"
}

smoke_script() {
  cat <<'EOF'
cd /vm
echo '$ file Image root.ext4'; file Image root.ext4
echo '$ e2fsck -fn root.ext4'; e2fsck -fn root.ext4
echo '$ dumpe2fs -h (selected)'
dumpe2fs -h root.ext4 2>/dev/null | grep -E '^(Filesystem (UUID|features|created)|Directory Hash Seed|Last mount time|Filesystem volume name):'
for f in /usr/local/sbin/agentkit-vm-init /usr/local/bin/agentkit-sandboxd /sbin/mkfs.ext4 /etc/hosts /etc/sandbox/environment; do
  printf '%s: ' "$f"; debugfs -R "stat $f" root.ext4 2>/dev/null | sed -n 's/^Inode: [0-9]*   Type: \([a-z]*\) *Mode: *\([0-7]*\).*/type \1 mode \2/p' | tr -d '\n'
  printf ' owner '; debugfs -R "stat $f" root.ext4 2>/dev/null | sed -n 's/^User: *\([0-9]*\) *Group: *\([0-9]*\).*/\1:\2/p'
done
debugfs -R 'dump /usr/local/sbin/agentkit-vm-init /tmp/init' root.ext4 2>/dev/null
cmp /tmp/init /src/usr/local/sbin/agentkit-vm-init && echo 'agentkit-vm-init on the disk equals vm/rootfs/usr/local/sbin/agentkit-vm-init'
echo '$ check-config.sh kernel/agentkit.config kernel.config'; sh /kernel/check-config.sh /kernel/agentkit.config kernel.config
echo '$ grep (required built-ins)'
grep -E '^CONFIG_(VIRTIO|VIRTIO_PCI|VIRTIO_BLK|EXT4_FS|VSOCKETS|VIRTIO_VSOCKETS|VIRTIO_BALLOON|HW_RANDOM_VIRTIO|VIRTIO_CONSOLE|DEVTMPFS|DEVTMPFS_MOUNT|TMPFS|PROC_FS|SYSFS|UNIX|INET|FUTEX|EPOLL|SIGNALFD|TIMERFD|EVENTFD|SHMEM|POSIX_MQUEUE)=y$' kernel.config
echo '$ grep (off)'
grep -E '^# CONFIG_(MODULES|BLK_DEV_INITRD|NETDEVICES|USER_NS) is not set$' kernel.config
EOF
}

cmd_clean() {
  rm -rf "$OUT/work"
  docker buildx prune --builder "$BUILDER" --force --filter 'until=0s' >/dev/null 2>&1 || true
}

cmd="${1:-all}"; shift || true
case "$cmd" in
  kernel) cmd_kernel "$@" ;;
  root) cmd_root "$@" ;;
  manifest) cmd_manifest ;;
  all) cmd_kernel; cmd_root; cmd_manifest ;;
  verify) cmd_verify ;;
  smoke) cmd_smoke ;;
  clean) cmd_clean ;;
  *) sed -n '2,11p' "$0"; exit 2 ;;
esac
