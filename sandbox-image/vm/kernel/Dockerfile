# check=skip=InvalidDefaultArgInFrom
# The microVM kernel (ADR 0040 §4): upstream longterm tarball, verified by
# sha256, allnoconfig plus agentkit.config, arm64 Image. Build only through
# vm/build.sh kernel: every ARG comes from ../manifest.env and
# vm/manifest.env. The kernel builds in BuildKit's own (case-sensitive)
# filesystem; only Image and the final .config are exported.

ARG BASE_IMAGE

FROM ${BASE_IMAGE} AS toolchain
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
 && apt-get -y --no-install-recommends install gcc make bc bison flex libc6-dev xz-utils \
 && rm -rf /var/lib/apt/lists/*

FROM toolchain AS build
ARG KERNEL_VERSION
ARG KERNEL_URL
ARG KERNEL_SHA256
ARG SOURCE_DATE_EPOCH
ADD --checksum=sha256:${KERNEL_SHA256} ${KERNEL_URL} /build/linux.tar.xz
COPY kernel/agentkit.config kernel/check-config.sh /build/
# Fixed path, user, host and timestamp, so the Image does not depend on
# where or when it was built.
RUN tar -xf /build/linux.tar.xz -C /build \
 && mv "/build/linux-${KERNEL_VERSION}" /build/linux \
 && cd /build/linux \
 && export ARCH=arm64 KBUILD_BUILD_USER=agentkit KBUILD_BUILD_HOST=agentkit KBUILD_BUILD_VERSION=1 \
           KBUILD_BUILD_TIMESTAMP="$(date -u -d "@${SOURCE_DATE_EPOCH}" '+%Y-%m-%d %H:%M:%S UTC')" \
 && make -s allnoconfig \
 && scripts/kconfig/merge_config.sh -m -O . .config /build/agentkit.config \
 && make -s olddefconfig \
 && sh /build/check-config.sh /build/agentkit.config .config \
 && make -s -j"$(nproc)" Image \
 && mkdir /out \
 && cp arch/arm64/boot/Image /out/Image \
 && cp .config /out/kernel.config

FROM scratch
COPY --from=build /out/ /
