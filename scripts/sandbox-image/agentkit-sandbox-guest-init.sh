#!/bin/sh
# agentkit-sandbox-guest-init.sh — inject/refresh the guest agent into an
# already-extracted rootfs tree (idempotent, admin-side).
#
#   ./agentkit-sandbox-guest-init.sh --root <dir> --agent-bin <path> \
#       [--agent-digest sha256:<hex>]
#
# Verifies the binary against --agent-digest when given (refuses on
# mismatch), installs it 0755 at /agentkit/agent, writes AGENT.sha256 and
# the IMAGE_META.json marker, and guarantees the /init exec contract and
# /workspace mount point.
set -eu

ROOT=""
AGENT=""
DIGEST=""
while [ $# -gt 0 ]; do
    case "$1" in
        --root) ROOT="$2"; shift 2 ;;
        --agent-bin) AGENT="$2"; shift 2 ;;
        --agent-digest) DIGEST="$2"; shift 2 ;;
        *) echo "unknown flag $1" >&2; exit 2 ;;
    esac
done
[ -n "$ROOT" ] && [ -d "$ROOT" ] || { echo "guest-init: --root <dir> required" >&2; exit 2; }
[ -n "$AGENT" ] && [ -f "$AGENT" ] || { echo "guest-init: --agent-bin required" >&2; exit 2; }

HEX="$(sha256sum "$AGENT" | awk '{print $1}')"
if [ -n "$DIGEST" ]; then
    WANT="${DIGEST#sha256:}"
    [ "$HEX" = "$WANT" ] || { echo "guest-init: agent digest mismatch (have $HEX want $WANT)" >&2; exit 1; }
fi

mkdir -p "$ROOT/agentkit" "$ROOT/workspace"
install -m 0755 "$AGENT" "$ROOT/agentkit/agent"
printf '%s\n' "$HEX" > "$ROOT/agentkit/AGENT.sha256"
[ -f "$ROOT/init" ] || {
    printf '#!/bin/sh\nexport PATH=/agentkit:/bin:/sbin:/usr/bin:/usr/sbin\nexec /agentkit/agent\n' > "$ROOT/init"
}
chmod 0755 "$ROOT/init"
printf '{"schema":1,"agent_sha256":"%s","built_by":"agentkit-sandbox-guest-init.sh"}\n' "$HEX" \
    > "$ROOT/agentkit/IMAGE_META.json"
echo "guest-init: agent $HEX installed into $ROOT"
