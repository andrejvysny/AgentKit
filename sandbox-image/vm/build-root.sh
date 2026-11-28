#!/usr/bin/env bash
# usage: vm/build-root.sh [--no-cache] [OUT_DIR]
# Build root.ext4 (ADR 0040 §4) from the arm64 platform of
# ../out/image.oci.tar (built with ../build.sh when missing) plus the VM
# layer in root/Dockerfile. Writes OUT_DIR/root.ext4 (default vm/out),
# OUT_DIR/root-oci-digest.txt (the index digest it was built from) and
# OUT_DIR/vm-packages.txt (Debian packages the VM layer added).
set -euo pipefail

VM="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE_DIR="$(cd "$VM/.." && pwd)"
# shellcheck source=../manifest.env
source "$IMAGE_DIR/manifest.env"
# shellcheck source=manifest.env
source "$VM/manifest.env"
export SOURCE_DATE_EPOCH
BUILDER=agentkit-sandbox-builder
OCI_TAR="$IMAGE_DIR/out/image.oci.tar"

log() { printf '==> %s\n' "$*" >&2; }

cache=()
if [[ "${1:-}" == "--no-cache" ]]; then cache=(--no-cache); shift; fi
OUT="${1:-$VM/out}"
WORK="$VM/out/work"
mkdir -p "$OUT" "$WORK"

ensure_oci_image() {
  [[ -f "$OCI_TAR" ]] && return 0
  log "$OCI_TAR is missing: building it with ../build.sh"
  local arch
  for arch in $(echo "$PLATFORMS" | tr ',' '\n' | sed 's#.*/##'); do
    [[ -f "$IMAGE_DIR/out/agentkit-sandboxd-$arch" ]] || { "$IMAGE_DIR/build.sh" sandboxd; break; }
  done
  "$IMAGE_DIR/build.sh" build >&2
}

# The index digest of the archive, read from the archive itself (the lead
# rebuilds it), after checking it has a linux/arm64 manifest.
index_digest() {
  python3 -I - "$OCI_TAR" <<'EOF'
import json, sys, tarfile
with tarfile.open(sys.argv[1]) as t:
    blob = lambda d: json.load(t.extractfile("blobs/sha256/" + d.split(":", 1)[1]))
    (top,) = json.load(t.extractfile("index.json"))["manifests"]
    platforms = [m["platform"] for m in blob(top["digest"])["manifests"]]
    assert {"os": "linux", "architecture": "arm64"} in platforms, platforms
    print(top["digest"])
EOF
}

ensure_builder() {
  if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
    log "creating buildx builder $BUILDER ($BUILDKIT_IMAGE)"
    docker buildx create --name "$BUILDER" --driver docker-container \
      --driver-opt "image=$BUILDKIT_IMAGE" --bootstrap >/dev/null
  fi
}

ensure_oci_image
digest="$(index_digest)"
rm -rf "$WORK/oci" "$WORK/root"
mkdir -p "$WORK/oci" "$WORK/root"
tar -xf "$OCI_TAR" -C "$WORK/oci"
ensure_builder
log "building root.ext4 from $digest (linux/arm64)"
docker buildx build --builder "$BUILDER" --platform linux/arm64 \
  --provenance=false --sbom=false ${cache[@]+"${cache[@]}"} \
  --build-context "sandbox=oci-layout://$WORK/oci@$digest" \
  --build-arg "BASE_IMAGE=$BASE_IMAGE" \
  --build-arg "DEBIAN_SNAPSHOT=$DEBIAN_SNAPSHOT" \
  --build-arg "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH" \
  --build-arg "ROOT_SIZE_MIB=$ROOT_SIZE_MIB" \
  --build-arg "ROOT_LABEL=$ROOT_LABEL" \
  --build-arg "ROOT_UUID=$ROOT_UUID" \
  --build-arg "ROOT_HASH_SEED=$ROOT_HASH_SEED" \
  --output "type=local,dest=$WORK/root" \
  -f "$VM/root/Dockerfile" "$VM"
mv "$WORK/root/root.ext4" "$OUT/root.ext4"
mv "$WORK/root/vm-packages.txt" "$OUT/vm-packages.txt"
echo "$digest" > "$OUT/root-oci-digest.txt"
log "e2fsck -fn: $(tail -1 "$WORK/root/e2fsck.txt")"
rm -rf "$WORK/oci" "$WORK/root"
