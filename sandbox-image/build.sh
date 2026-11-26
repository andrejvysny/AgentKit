#!/usr/bin/env bash
# Build, verify and test the agent sandbox image from manifest.env.
#
#   ./build.sh sandboxd                  build out/agentkit-sandboxd-<arch> from ../rust in RUST_IMAGE,
#                                        print its sha256 and compare it with manifest.env
#   ./build.sh build [--no-cache] [OUT]  OCI archive for all PLATFORMS (default out/image.oci.tar)
#   ./build.sh verify                    build twice (second: fresh builder, --no-cache),
#                                        compare digests, record them in lock/image-digests.txt
#   ./build.sh finalize IN OUT           wrap a one-platform OCI archive in an index and name it
#                                        localhost/<name>:<version> (build runs it)
#   ./build.sh load [OCI]                load each platform into docker as agentkit-sandbox:<version>-<arch>
#   ./build.sh selftest [ARCH...]        run sandbox-selftest (default: every platform)
#   ./build.sh notice                    extract NOTICE and dpkg versions per platform from loaded images
#   ./build.sh lock-pip                  re-resolve requirements.lock from requirements.in (needs uv)
#   ./build.sh clean                     remove the dedicated buildx builder, its cache and the cargo cache
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"
# shellcheck source=manifest.env
source ./manifest.env
export SOURCE_DATE_EPOCH  # buildx reads it from the environment too
BUILDER=agentkit-sandbox-builder
OUT_DIR="$HERE/out"
mkdir -p "$OUT_DIR"

log() { printf '==> %s\n' "$*" >&2; }

platform_archs() { echo "$PLATFORMS" | tr ',' '\n' | sed 's#.*/##'; }

sha256_of() { python3 -I -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$1"; }

# The pinned sha256 of agentkit-sandboxd for ARCH (SANDBOXD_SHA256_<ARCH>).
sandboxd_pin() {
  local var
  var="SANDBOXD_SHA256_$(echo "$1" | tr '[:lower:]' '[:upper:]')"
  echo "${!var:-}"
}

ensure_builder() {
  if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
    log "creating buildx builder $BUILDER ($BUILDKIT_IMAGE)"
    docker buildx create --name "$BUILDER" --driver docker-container \
      --driver-opt "image=$BUILDKIT_IMAGE" --bootstrap >/dev/null
  fi
}

# RUST_IMAGE's variant for ARCH, by its manifest digest in the pinned
# index. Docker's classic image store keeps one platform per digest
# reference (a second platform of the same index fails with "cannot
# overwrite digest"), so each platform runs by its own manifest digest. The
# index is fetched by digest and its sha256 checked here, so the entry is
# the pinned index's own.
rust_image_for() {
  local digest
  digest="$(docker buildx imagetools inspect --raw "$RUST_IMAGE" | python3 -I -c '
import hashlib, json, sys
raw, want, arch = sys.stdin.buffer.read(), sys.argv[1], sys.argv[2]
assert "sha256:" + hashlib.sha256(raw).hexdigest() == want, "the index does not match " + want
(digest,) = [m["digest"] for m in json.loads(raw)["manifests"]
             if m.get("platform", {}).get("os") == "linux" and m["platform"].get("architecture") == arch]
print(digest)' "${RUST_IMAGE##*@}" "$1")"
  echo "${RUST_IMAGE%@*}@$digest"
}

# agentkit-sandboxd for each platform: a static musl binary built from
# ../rust in that platform's variant of RUST_IMAGE (native on its own
# architecture, emulated otherwise). The crate registry is fetched with
# network into a cache volume; the build itself runs with --network none
# and --offline. RUSTUP_TOOLCHAIN overrides rust/rust-toolchain.toml, whose
# clippy and rustfmt components the image lacks, so rustup never fetches
# anything. Fixed paths (/src, /target) keep the host's checkout path out
# of the binary.
cmd_sandboxd() {
  local rust_dir channel arch image
  rust_dir="$(cd "$HERE/../rust" && pwd)"
  channel="$(sed -n 's/^channel *= *"\(.*\)"$/\1/p' "$rust_dir/rust-toolchain.toml")"
  for arch in $(platform_archs); do
    case "$arch" in
      amd64|arm64) ;;
      *) log "no agentkit-sandboxd build or pin for $arch (the Dockerfile knows amd64 and arm64)"; return 1 ;;
    esac
    local target="$OUT_DIR/sandboxd-target-$arch" bin="$OUT_DIR/agentkit-sandboxd-$arch" got want
    mkdir -p "$target"
    image="$(rust_image_for "$arch")"
    local run=(docker run --rm --platform "linux/$arch" -v "$rust_dir:/src:ro" -v "$target:/target"
      -v agentkit-sandboxd-cargo:/usr/local/cargo/registry -w /src
      -e CARGO_TARGET_DIR=/target -e "RUSTUP_TOOLCHAIN=$channel")
    log "fetching crates for agentkit-sandboxd ($arch, $image)"
    "${run[@]}" "$image" cargo fetch --locked >&2
    log "building agentkit-sandboxd ($arch, offline)"
    "${run[@]}" --network none "$image" \
      cargo build --locked --offline --release -p agentkit-sandboxd >&2
    cp "$target/release/agentkit-sandboxd" "$bin"
    got="$(sha256_of "$bin")"; want="$(sandboxd_pin "$arch")"
    echo "agentkit-sandboxd-$arch $got"
    [[ "$got" == "$want" ]] || log "manifest.env pins '$want' for $arch: update its SANDBOXD_SHA256_ line after review"
  done
}

# Print "index <digest>" and "<os/arch> <manifest digest> <config digest>" lines.
oci_digests() {
  python3 -I - "$1" <<'EOF'
import json, sys, tarfile
with tarfile.open(sys.argv[1]) as t:
    blob = lambda d: json.load(t.extractfile("blobs/sha256/" + d.split(":", 1)[1]))
    top = json.load(t.extractfile("index.json"))["manifests"]
    assert len(top) == 1, top
    print("index", top[0]["digest"])
    for m in sorted(blob(top[0]["digest"])["manifests"], key=lambda m: m["platform"]["architecture"]):
        p = m["platform"]
        print("%s/%s %s %s" % (p["os"], p["architecture"], m["digest"], blob(m["digest"])["config"]["digest"]))
EOF
}

# BuildKit writes a one-platform build as a bare manifest; wrap it in an
# index (as a multi-platform build has) so the image is always referenced
# by index digest, and name it in index.json (io.containerd.image.name for
# docker, org.opencontainers.image.ref.name for podman). The archive is
# rewritten in a fixed order with fixed metadata; no blob changes.
cmd_finalize() {
  local in="$1" out="$2"
  python3 -I - "$in" "$out" "localhost/$IMAGE_NAME:$IMAGE_VERSION" "$SOURCE_DATE_EPOCH" <<'EOF'
import hashlib, io, json, sys, tarfile
src, dst, name, epoch = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
INDEX = "application/vnd.oci.image.index.v1+json"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
blob_path = lambda d: "blobs/sha256/" + d.split(":", 1)[1]
with tarfile.open(src) as t:
    files = {m.name.lstrip("./"): m for m in t.getmembers() if m.isfile()}
    read = lambda path: t.extractfile(files[path]).read()
    top = json.loads(read("index.json"))
    (desc,) = top["manifests"]
    extra = {}
    if desc["mediaType"] == MANIFEST:
        manifest = json.loads(read(blob_path(desc["digest"])))
        config = json.loads(read(blob_path(manifest["config"]["digest"])))
        platform = {"architecture": config["architecture"], "os": config["os"]}
        inner = {"mediaType": MANIFEST, "digest": desc["digest"], "size": desc["size"], "platform": platform}
        body = json.dumps({"schemaVersion": 2, "mediaType": INDEX, "manifests": [inner]},
                          separators=(",", ":")).encode()
        digest = "sha256:" + hashlib.sha256(body).hexdigest()
        extra[blob_path(digest)] = body
        desc = {"mediaType": INDEX, "digest": digest, "size": len(body)}
    assert desc["mediaType"] == INDEX, desc
    desc = {k: v for k, v in desc.items() if k != "annotations"}
    desc["annotations"] = {"io.containerd.image.name": name, "org.opencontainers.image.ref.name": name}
    top["manifests"] = [desc]
    extra["index.json"] = json.dumps(top, separators=(",", ":")).encode()
    names = sorted(set(files) | set(extra), key=lambda n: (n.startswith("blobs/"), n))
    with tarfile.open(dst, "w", format=tarfile.USTAR_FORMAT) as out:
        def add(info, data=None):
            info.mtime, info.uid, info.gid, info.uname, info.gname = epoch, 0, 0, "", ""
            out.addfile(info, data)
        for d in ("blobs", "blobs/sha256"):
            info = tarfile.TarInfo(d); info.type, info.mode = tarfile.DIRTYPE, 0o755; add(info)
        for n in names:
            info = tarfile.TarInfo(n); info.mode = 0o644
            if n in extra:
                info.size = len(extra[n]); add(info, io.BytesIO(extra[n]))
            else:
                info.size = files[n].size; add(info, t.extractfile(files[n]))
EOF
}

cmd_build() {
  local cache=() out="$OUT_DIR/image.oci.tar" arch
  if [[ "${1:-}" == "--no-cache" ]]; then cache=(--no-cache); shift; fi
  [[ -n "${1:-}" ]] && out="$1"
  mkdir -p "$(dirname "$out")"
  for arch in $(platform_archs); do
    if [[ ! -f "$OUT_DIR/agentkit-sandboxd-$arch" ]]; then
      log "out/agentkit-sandboxd-$arch is missing: run ./build.sh sandboxd first"; return 1
    fi
  done
  ensure_builder
  log "building $IMAGE_NAME:$IMAGE_VERSION for $PLATFORMS -> $out"
  docker buildx build \
    --builder "$BUILDER" \
    --platform "$PLATFORMS" \
    --provenance=false --sbom=false \
    ${cache[@]+"${cache[@]}"} \
    --build-arg "BASE_IMAGE=$BASE_IMAGE" \
    --build-arg "DEBIAN_SNAPSHOT=$DEBIAN_SNAPSHOT" \
    --build-arg "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH" \
    --build-arg "NODE_VERSION=$NODE_VERSION" \
    --build-arg "NODE_SHA256_ARM64=$NODE_SHA256_ARM64" \
    --build-arg "NODE_SHA256_AMD64=$NODE_SHA256_AMD64" \
    --build-arg "SANDBOXD_SHA256_AMD64=$SANDBOXD_SHA256_AMD64" \
    --build-arg "SANDBOXD_SHA256_ARM64=$SANDBOXD_SHA256_ARM64" \
    --output "type=oci,dest=$out.buildkit,rewrite-timestamp=true" \
    "$HERE"
  cmd_finalize "$out.buildkit" "$out"
  rm -f "$out.buildkit"
  oci_digests "$out"
}

cmd_verify() {
  local a="$OUT_DIR/build-1.oci.tar" b="$OUT_DIR/build-2.oci.tar"
  cmd_build "$a" > "$OUT_DIR/build-1.digests" || return 1
  log "recreating builder for a cold second build"
  docker buildx rm "$BUILDER" >/dev/null 2>&1 || true
  cmd_build --no-cache "$b" > "$OUT_DIR/build-2.digests"
  log "build 1:"; cat "$OUT_DIR/build-1.digests" >&2
  log "build 2:"; cat "$OUT_DIR/build-2.digests" >&2
  if ! cmp -s "$OUT_DIR/build-1.digests" "$OUT_DIR/build-2.digests"; then
    log "DIGESTS DIFFER"; return 1
  fi
  {
    echo "# Output of ./build.sh verify: two builds from manifest.env, the second cold"
    echo "# (fresh builder, --no-cache), produced these identical digests."
    echo "# Columns: platform, image manifest digest, image config digest (= docker image id)."
    cat "$OUT_DIR/build-1.digests"
  } > lock/image-digests.txt
  cp "$a" "$OUT_DIR/image.oci.tar"
  log "reproducible: digests match; recorded in lock/image-digests.txt"
}

# docker's classic image store loads a docker-archive (manifest.json) with
# one platform per archive. Write that manifest next to the OCI blobs for
# each platform; blobs, and so the config digest (= image id), are unchanged.
cmd_load() {
  local oci="${1:-$OUT_DIR/image.oci.tar}"
  local work; work="$(mktemp -d "$OUT_DIR/load.XXXXXX")"
  trap 'rm -rf "$work"' RETURN
  tar -xf "$oci" -C "$work"
  local arch
  for arch in $(platform_archs); do
    python3 -I - "$work" "$arch" "$IMAGE_NAME:$IMAGE_VERSION-$arch" <<'EOF'
import json, sys, os
root, arch, ref = sys.argv[1:]
blob = lambda d: os.path.join("blobs/sha256", d.split(":")[1])
load = lambda d: json.load(open(os.path.join(root, blob(d))))
top = json.load(open(os.path.join(root, "index.json")))["manifests"][0]
m = [m for m in load(top["digest"])["manifests"] if m["platform"]["architecture"] == arch][0]
manifest = load(m["digest"])
json.dump([{"Config": blob(manifest["config"]["digest"]), "RepoTags": [ref],
            "Layers": [blob(layer["digest"]) for layer in manifest["layers"]]}],
          open(os.path.join(root, "manifest.json"), "w"))
EOF
    (cd "$work" && tar -cf - manifest.json blobs) | docker load >&2
  done
}

expected_config() {
  [[ -f lock/image-digests.txt ]] || return 0
  awk -v p="linux/$1" '$1 == p { print $3 }' lock/image-digests.txt
}

cmd_selftest() {
  local archs=("$@"); [[ ${#archs[@]} -eq 0 ]] && read -r -a archs <<< "$(platform_archs | tr '\n' ' ')"
  local arch rc=0
  for arch in "${archs[@]}"; do
    local ref="$IMAGE_NAME:$IMAGE_VERSION-$arch" want got
    want="$(expected_config "$arch")"
    got="$(docker image inspect --format '{{.Id}}' "$ref")"
    if [[ -n "$want" && "$want" != "$got" ]]; then
      log "$ref is $got, lock/image-digests.txt says $want"; return 1
    fi
    log "selftest $ref ($got)"
    local ws="agentkit-sandbox-selftest-ws-$$" home="agentkit-sandbox-selftest-home-$$"
    docker volume create "$ws" >/dev/null; docker volume create "$home" >/dev/null
    docker run --rm --platform "linux/$arch" --init \
      --network none --read-only --tmpfs /tmp:rw,nosuid,nodev,size=512m \
      --user 1000:1000 --cap-drop ALL --security-opt no-new-privileges \
      -v "$ws:/workspace" -v "$home:/home/agent" \
      --entrypoint sandbox-selftest "$ref" || rc=1
    docker volume rm "$ws" "$home" >/dev/null
  done
  return "$rc"
}

cmd_notice() {
  local arch
  for arch in $(platform_archs); do
    local ref="$IMAGE_NAME:$IMAGE_VERSION-$arch"
    docker run --rm --platform "linux/$arch" --network none --entrypoint cat "$ref" \
      /usr/share/doc/sandbox-image/NOTICE > "notice/NOTICE-linux-$arch.txt"
    docker run --rm --platform "linux/$arch" --network none --entrypoint dpkg-query "$ref" \
      -W -f '${Package}=${Version}\n' | LC_ALL=C sort > "lock/dpkg-linux-$arch.txt"
    log "wrote notice/NOTICE-linux-$arch.txt and lock/dpkg-linux-$arch.txt"
  done
}

cmd_lock_pip() {
  {
    echo "# Hash-locked PyPI wheels for the image (python $PYTHON_VERSION, linux arm64 and amd64)."
    echo "# Generated by ./build.sh lock-pip from requirements.in; do not edit by hand."
    uv pip compile requirements.in --universal --python-version "$PYTHON_VERSION" \
      --only-binary :all: --exclude-newer "$PIP_EXCLUDE_NEWER" --generate-hashes \
      --no-header --quiet
  } > requirements.lock
  log "wrote requirements.lock"
}

cmd_clean() {
  docker buildx rm "$BUILDER" 2>/dev/null || true
  docker volume rm agentkit-sandboxd-cargo >/dev/null 2>&1 || true
  rm -rf "$OUT_DIR"/sandboxd-target-*
}

cmd="${1:-build}"; shift || true
case "$cmd" in
  sandboxd) cmd_sandboxd ;;
  build) cmd_build "$@" ;;
  finalize) cmd_finalize "$@" ;;
  verify) cmd_verify ;;
  load) cmd_load "$@" ;;
  selftest) cmd_selftest "$@" ;;
  notice) cmd_notice ;;
  lock-pip) cmd_lock_pip ;;
  clean) cmd_clean ;;
  *) sed -n '2,15p' "$0"; exit 2 ;;
esac
