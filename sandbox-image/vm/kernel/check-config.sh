#!/bin/sh
# usage: check-config.sh FRAGMENT CONFIG
# Fails unless every setting of FRAGMENT holds in the final CONFIG: a
# "CONFIG_X=v" line must appear verbatim, and "# CONFIG_X is not set" means
# CONFIG_X has no "=" line (kconfig writes a disabled option either as that
# comment or not at all).
set -eu
fragment=$1
config=$2
bad=0
while IFS= read -r line; do
  case $line in
    CONFIG_*=*)
      grep -qxF -- "$line" "$config" || { echo "missing: $line" >&2; bad=1; } ;;
    "# CONFIG_"*" is not set")
      name=${line#\# }; name=${name%% *}
      if grep -q "^$name=" "$config"; then
        echo "should be off: $(grep "^$name=" "$config")" >&2; bad=1
      fi ;;
  esac
done < "$fragment"
[ "$bad" -eq 0 ] && echo "check-config: every setting of $fragment holds"
exit "$bad"
