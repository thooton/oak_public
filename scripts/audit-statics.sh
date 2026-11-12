#!/bin/sh
# Lists the statics and thread-locals of the crates whose values cross
# between Oak and a plugin (egui, epaint, emath, ecolor) and of
# parking_lot_core: state there is per copy of the crate, and a plugin has
# its own copy (docs/PLUGINS.md, vendor/README.md). Run it when moving to a
# new egui and compare with what vendor/README.md says was checked.
#
#   scripts/audit-statics.sh [version]     (default: the one in Cargo.lock)
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
version=${1:-$(awk '/^name = "egui"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$here/Cargo.lock")}
registry=${CARGO_HOME:-$HOME/.cargo}/registry/src
found=
for crate in egui epaint emath ecolor; do
    dir=$(ls -d "$registry"/*/"$crate-$version" 2>/dev/null | head -1 || true)
    [ "$crate" = epaint ] && dir=$here/vendor/epaint
    if [ -z "$dir" ] || [ ! -d "$dir" ]; then
        echo "$crate $version: not in $registry (cargo fetch first)" >&2
        continue
    fi
    found=1
    echo "== $crate ($dir)"
    lines=$(grep -rnE '^\s*(pub(\([a-z]+\))? )?static (mut )?[A-Z_]+|thread_local!|OnceLock<|LazyLock<|lazy_static!|AtomicU(size|32|64)::new' "$dir/src" | sed "s|$dir/||" || true)
    echo "${lines:-(none)}"
done
echo "== parking_lot_core ($here/vendor/parking_lot_core)"
grep -rnE '^\s*(pub(\([a-z]+\))? )?static (mut )?[A-Z_]+|thread_local!' "$here/vendor/parking_lot_core/src" | sed "s|$here/vendor/parking_lot_core/||"
[ -n "$found" ] || exit 1
