# check=skip=InvalidDefaultArgInFrom
# The microVM root disk (ADR 0040 §4-§5): the arm64 platform of the
# agent sandbox OCI image plus the VM layer (e2fsprogs from the pinned
# Debian snapshot, agentkit-vm-init, /etc/hosts), written into an ext4 image
# with mkfs.ext4 -d. Build only through vm/build-root.sh: the build context
# `sandbox` is ../out/image.oci.tar by index digest, every ARG comes from
# ../manifest.env and vm/manifest.env.

ARG BASE_IMAGE

# --- the VM layer -----------------------------------------------------------
FROM sandbox AS vm
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
USER root
ARG DEBIAN_FRONTEND=noninteractive
# The guest supervisor in the image must be the pinned one
# (SANDBOXD_SHA256_ARM64 in ../manifest.env), whatever image.oci.tar is.
ARG SANDBOXD_SHA256
RUN test -n "${SANDBOXD_SHA256}" \
 && echo "${SANDBOXD_SHA256}  /usr/local/bin/agentkit-sandboxd" | sha256sum --check --strict
# The image's apt sources already name the snapshot. Files whose content
# depends on the build time are removed as in ../Dockerfile; the packages
# the layer adds are recorded for the NOTICE.
RUN dpkg-query -W -f '${Package}=${Version}\n' | LC_ALL=C sort > /tmp/before \
 && apt-get update \
 && apt-get -y --no-install-recommends install e2fsprogs \
 && apt-get clean \
 && rm -rf /var/lib/apt/lists/* /var/cache/apt/* /var/log/* /var/cache/ldconfig/aux-cache \
           /var/cache/debconf/*-old /var/lib/dpkg/*-old /root/.cache \
 && dpkg-query -W -f '${Package}=${Version}\n' | LC_ALL=C sort > /tmp/after \
 && LC_ALL=C comm -13 /tmp/before /tmp/after > /usr/share/doc/sandbox-image/vm-packages.txt \
 && rm /tmp/before /tmp/after
COPY --chmod=0755 rootfs/usr/local/sbin/agentkit-vm-init /usr/local/sbin/agentkit-vm-init
# A container runtime writes /etc/hosts; the VM's read-only root carries it.
COPY --chmod=0644 root/hosts /etc/hosts
RUN find / -xdev -perm /6000 -type f -exec chmod ug-s {} + \
 && test -z "$(find / -xdev -perm /6000 -type f)" \
 && test -z "$(find / -xdev -user 1000 ! -path /workspace ! -path /home/agent)"

# --- mkfs tools (not part of the disk) ---------------------------------------
FROM ${BASE_IMAGE} AS mkfs
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
ARG DEBIAN_SNAPSHOT
ARG DEBIAN_FRONTEND=noninteractive
RUN rm -f /etc/apt/sources.list.d/debian.sources \
 && { echo "deb http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/ trixie main"; \
      echo "deb http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/ trixie-updates main"; \
      echo "deb http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/ trixie-security main"; \
    } > /etc/apt/sources.list \
 && echo 'Acquire::Check-Valid-Until "false"; Acquire::Retries "5";' > /etc/apt/apt.conf.d/50snapshot \
 && apt-get update \
 && apt-get -y --no-install-recommends install e2fsprogs libarchive13t64 \
 && rm -rf /var/lib/apt/lists/*

# Tools for `vm/build.sh smoke` (not part of the disk).
FROM mkfs AS tools
RUN apt-get update \
 && apt-get -y --no-install-recommends install file \
 && rm -rf /var/lib/apt/lists/*

# --- the disk ---------------------------------------------------------------
# mkfs.ext4 -d reads a tar archive, not the tree: from a directory it would
# copy each file's ctime and atime (set when BuildKit unpacked the layers)
# and depend on readdir order. The archive is sorted, mtimes are clamped to
# SOURCE_DATE_EPOCH, atime and ctime are dropped; E2FSPROGS_FAKE_TIME fixes
# the filesystem's own times (libarchive needs a UTF-8 locale for the
# non-ASCII names in the image). No journal: the root is mounted read-only.
FROM mkfs AS disk
ARG SOURCE_DATE_EPOCH
ARG ROOT_SIZE_MIB
ARG ROOT_LABEL
ARG ROOT_UUID
ARG ROOT_HASH_SEED
RUN --mount=type=bind,from=vm,source=/,target=/rootfs \
    mkdir /out \
 && tar -C /rootfs --sort=name --format=pax --numeric-owner \
        --mtime="@${SOURCE_DATE_EPOCH}" --clamp-mtime \
        --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
        -cf /tmp/root.tar . \
 && LC_ALL=C.UTF-8 E2FSPROGS_FAKE_TIME="${SOURCE_DATE_EPOCH}" mkfs.ext4 -q -F -t ext4 -b 4096 -m 0 \
      -L "${ROOT_LABEL}" -U "${ROOT_UUID}" -O ^has_journal \
      -E "root_owner=0:0,hash_seed=${ROOT_HASH_SEED},nodiscard" \
      -d /tmp/root.tar /out/root.ext4 "${ROOT_SIZE_MIB}M" \
 && rm /tmp/root.tar \
 && e2fsck -fn /out/root.ext4 > /out/e2fsck.txt \
 && cp /rootfs/usr/share/doc/sandbox-image/vm-packages.txt /out/vm-packages.txt

FROM scratch
COPY --from=disk /out/ /
