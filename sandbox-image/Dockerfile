# check=skip=InvalidDefaultArgInFrom
# Agent sandbox image v1. Build only through build.sh: every ARG below
# comes from manifest.env, and reproducibility relies on the OCI exporter
# flags build.sh passes (rewrite-timestamp, no attestations).
# v1: the guest supervisor agentkit-sandboxd (built by `build.sh sandboxd`,
# pinned by sha256) is the entrypoint, and the command environment and npm
# point at its egress shim (ADR 0039).

ARG BASE_IMAGE

FROM ${BASE_IMAGE} AS base
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]

# --- Node.js: official tarball, verified against the pinned sha256 ---------
FROM base AS node-arm64
ARG NODE_VERSION
ARG NODE_SHA256_ARM64
ADD --checksum=sha256:${NODE_SHA256_ARM64} https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-arm64.tar.gz /node.tar.gz

FROM base AS node-amd64
ARG NODE_VERSION
ARG NODE_SHA256_AMD64
ADD --checksum=sha256:${NODE_SHA256_AMD64} https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.gz /node.tar.gz

FROM node-${TARGETARCH} AS node
RUN mkdir /out \
 && tar -xzf /node.tar.gz -C /out --strip-components=1 --no-same-owner --no-same-permissions \
 && mkdir -p /out/share/doc/nodejs \
 && mv /out/LICENSE /out/share/doc/nodejs/LICENSE \
 && rm -f /out/CHANGELOG.md /out/README.md \
 && rm -rf /out/include /out/share/doc/node /out/share/man

# --- the image --------------------------------------------------------------
FROM base AS image
ARG DEBIAN_SNAPSHOT
ARG SOURCE_DATE_EPOCH
ARG DEBIAN_FRONTEND=noninteractive

# System packages from the pinned snapshot. Logs, caches and other files
# whose content depends on the build time are removed in the same layer.
RUN --mount=type=bind,source=apt-packages.txt,target=/tmp/apt-packages.txt \
    rm -f /etc/apt/sources.list.d/debian.sources \
 && { echo "deb http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/ trixie main"; \
      echo "deb http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/ trixie-updates main"; \
      echo "deb http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/ trixie-security main"; \
    } > /etc/apt/sources.list \
 && echo 'Acquire::Check-Valid-Until "false"; Acquire::Retries "5";' > /etc/apt/apt.conf.d/50snapshot \
 && apt-get update \
 && apt-get -y --no-install-recommends upgrade \
 && grep -v '^#' /tmp/apt-packages.txt | xargs apt-get -y --no-install-recommends install \
 && apt-get clean \
 && rm -rf /var/lib/apt/lists/* /var/cache/apt/* /var/log/* /var/cache/ldconfig/aux-cache \
           /var/cache/debconf/*-old /var/lib/dpkg/*-old /root/.cache \
 && rm -f /etc/machine-id /var/lib/dbus/machine-id \
 && test "$(python3 -c 'import sys; print("%d.%d" % sys.version_info[:2])')" = 3.13

# Python libraries: hash-locked wheels only, into /usr/local (the system
# site that uid 1000 cannot write). Removing EXTERNALLY-MANAGED lets the
# agent's `pip install` fall back to a user install under ~/.local.
# --ignore-installed: Debian's own copies (python3-packaging, a pip
# dependency) stay in /usr/lib; /usr/local comes first on sys.path.
RUN --mount=type=bind,source=requirements.lock,target=/tmp/requirements.lock \
    rm -f /usr/lib/python3.*/EXTERNALLY-MANAGED \
 && python3 -m pip install --no-cache-dir --root-user-action=ignore --disable-pip-version-check \
      --require-hashes --only-binary=:all: --no-deps --no-compile --ignore-installed -r /tmp/requirements.lock \
 && mkdir -p /usr/share/doc/sandbox-image \
 && cp /tmp/requirements.lock /usr/share/doc/sandbox-image/requirements.lock \
 && rm -rf /root/.cache

COPY --from=node /out/ /usr/local/
# npm also reads HTTPS_PROXY; the builtin config keeps the shim when a
# program clears the environment.
RUN printf 'prefix=/home/agent/.npm-global\nupdate-notifier=false\nfund=false\nproxy=http://127.0.0.1:3128\nhttps-proxy=http://127.0.0.1:3128\n' \
      > /usr/local/lib/node_modules/npm/npmrc \
 && rm -rf /root/.npm

# --chmod also applies to directories COPY creates, so create them first.
RUN install -d -m 0755 /etc/sandbox /usr/local/lib/sandbox
COPY --chmod=0644 rootfs/etc/sandbox/environment rootfs/etc/sandbox/bashrc /etc/sandbox/
COPY --chmod=0644 helpers/sandboxlib.py /usr/local/lib/sandbox/
COPY --chmod=0755 helpers/sandbox-exec helpers/sandbox-kill helpers/sandbox-fs helpers/sandbox-selftest /usr/local/bin/

# The guest supervisor, PID 1 of a server sandbox: a static musl binary
# that build.sh builds from ../rust and manifest.env pins by sha256.
ARG TARGETARCH
ARG SANDBOXD_SHA256_AMD64
ARG SANDBOXD_SHA256_ARM64
COPY --chmod=0755 out/agentkit-sandboxd-${TARGETARCH} /usr/local/bin/agentkit-sandboxd
RUN case "$TARGETARCH" in \
      amd64) want="$SANDBOXD_SHA256_AMD64" ;; \
      arm64) want="$SANDBOXD_SHA256_ARM64" ;; \
      *) echo "no agentkit-sandboxd pin for $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && echo "$want  /usr/local/bin/agentkit-sandboxd" | sha256sum --check --strict -

# Bytecode: drop every .pyc (pip's and dpkg's embed source mtimes) and
# recompile with hashes that are never re-checked; the root is read-only.
# One process (-j1) in sorted order: marshal flags an object shared when
# its refcount is above one, and a worker's refcounts depend on the files
# it compiled before, which -j0 hands out by timing (a cold rebuild changed
# one .pyc). The hash seed is fixed so nothing else varies between runs.
RUN find /usr/lib/python3 /usr/lib/python3.13 /usr/local/lib/python3.13 -name __pycache__ -type d -prune -exec rm -rf {} + \
 && PYTHONHASHSEED=0 python3 -m compileall -q -j1 --invalidation-mode unchecked-hash \
      /usr/lib/python3.13 /usr/lib/python3/dist-packages /usr/local/lib/python3.13/dist-packages /usr/local/lib/sandbox

# The agent user (uid/gid 1000), written directly so no field carries the
# build date. No sudo; setuid/setgid bits are stripped and the build fails
# if any remain.
RUN echo 'agent:x:1000:1000:agent:/home/agent:/bin/bash' >> /etc/passwd \
 && echo 'agent:x:1000:' >> /etc/group \
 && echo 'agent:!::0:99999:7:::' >> /etc/shadow \
 && echo 'agent:!::' >> /etc/gshadow \
 && install -d -o 1000 -g 1000 -m 0755 /workspace /home/agent \
 && find / -xdev -perm /6000 -type f -exec chmod ug-s {} + \
 && test -z "$(find / -xdev -perm /6000 -type f)" \
 && test -z "$(find / -xdev -user 1000 ! -path /workspace ! -path /home/agent)"

# Third-party notice, generated from what is installed, plus the crates
# and libraries linked into agentkit-sandboxd (a committed list).
RUN --mount=type=bind,source=notice/generate-notice.py,target=/tmp/generate-notice.py \
    --mount=type=bind,source=notice/agentkit-sandboxd-crates.txt,target=/tmp/agentkit-sandboxd-crates.txt \
    LC_ALL=C python3 -I /tmp/generate-notice.py /tmp/agentkit-sandboxd-crates.txt > /usr/share/doc/sandbox-image/NOTICE

ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/home/agent/.local/bin:/home/agent/.npm-global/bin \
    HOME=/home/agent \
    LANG=C.UTF-8 \
    PYTHONUSERBASE=/home/agent/.local \
    NPM_CONFIG_PREFIX=/home/agent/.npm-global \
    BASH_ENV=/etc/sandbox/bashrc
LABEL org.opencontainers.image.title="agentkit-sandbox" \
      org.opencontainers.image.version="v1"
USER 1000:1000
WORKDIR /workspace
# The launcher sets the same entrypoint and an empty command; the image
# has no CMD, so nothing is appended to the supervisor's argv.
ENTRYPOINT ["/usr/local/bin/agentkit-sandboxd"]
