#!/bin/sh
# usage: check-config.sh FRAGMENT CONFIG [SRCTREE]
# Fails unless every setting of FRAGMENT holds in the final CONFIG: a
# "CONFIG_X=v" line must appear verbatim, and "# CONFIG_X is not set" means
# CONFIG_X has no "=" line (kconfig writes a disabled option either as that
# comment or not at all). With SRCTREE (the kernel source), every symbol
# FRAGMENT names must also be defined by a Kconfig file an arm64 build reads,
# so a misspelt or removed option cannot pass as "not set".
set -eu
fragment=$1
config=$2
tree=${3:-}
bad=0
symbols=
if [ -n "$tree" ]; then
  symbols=$(mktemp)
  trap 'rm -f "$symbols"' EXIT
  find "$tree" -type f -name 'Kconfig*' \
    \( ! -path "$tree/arch/*" -o -path "$tree/arch/Kconfig*" -o -path "$tree/arch/arm64/*" \) \
    -exec sed -n 's/^[[:space:]]*\(menu\)\{0,1\}config[[:space:]][[:space:]]*\([A-Za-z0-9_][A-Za-z0-9_]*\)[[:space:]]*\(#.*\)\{0,1\}$/CONFIG_\2/p' {} + \
    | sort -u > "$symbols"
  [ -s "$symbols" ] || { echo "no Kconfig symbols found below $tree" >&2; exit 1; }
fi
defined() {
  [ -z "$symbols" ] || grep -qxF -- "$1" "$symbols" || { echo "not a Kconfig symbol: $1" >&2; bad=1; }
}
while IFS= read -r line; do
  case $line in
    CONFIG_*=*)
      defined "${line%%=*}"
      grep -qxF -- "$line" "$config" || { echo "missing: $line" >&2; bad=1; } ;;
    "# CONFIG_"*" is not set")
      name=${line#\# }; name=${name%% *}
      defined "$name"
      if grep -q "^$name=" "$config"; then
        echo "should be off: $(grep "^$name=" "$config")" >&2; bad=1
      fi ;;
  esac
done < "$fragment"
[ "$bad" -eq 0 ] && echo "check-config: every setting of $fragment holds"
exit "$bad"
