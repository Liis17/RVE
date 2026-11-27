#!/usr/bin/env bash
# Shares compiled third-party crates between worktrees through one cache
# directory (RVE_BUILD_CACHE, same volume as the worktree). Run it before the
# first build in a worktree and again after a successful build: one call pulls
# units missing in build/ and pushes units missing in the cache.
#
# Files in deps/ are hardlinked, so they take disk space once; a unit rebuilt
# with other inputs gets a new hash and a new local file. Workspace crates and
# incremental data are never shared: Cargo rebuilds them per checkout anyway.
set -euo pipefail
export LC_ALL=C

cache=${RVE_BUILD_CACHE:-}
if [ -z "$cache" ]; then
    echo "RVE_BUILD_CACHE is not set; skipping build cache sync"
    exit 0
fi

root=$(cd "$(dirname "$0")/.." && pwd)
build=$root/build
workspace=$(cd "$root" && cargo tree --workspace --depth 0 --prefix none --offline |
    awk 'NF { print $1 }' | sort -u)

# sync SRC DST: add third-party units that DST does not have yet.
sync() {
    local src=$1 dst=$2 profile from to units new unit
    for profile in "$src"/*/; do
        [ -d "$profile.fingerprint" ] || continue
        profile=$(basename "$profile")
        from=$src/$profile
        to=$dst/$profile
        mkdir -p "$to/deps" "$to/build" "$to/.fingerprint"

        # A unit is <package>-<hash>; the same hash names its files in deps/.
        units=$(awk 'NR == FNR { skip[$0] = 1; next }
            { pkg = $0; sub(/-[0-9a-f]+$/, "", pkg); if (!(pkg in skip)) print }' \
            <(echo "$workspace") <(ls "$from/.fingerprint"))
        [ -n "$units" ] || continue

        comm -23 \
            <(ls "$from/deps" | grep -Ff <(echo "$units" | sed -E 's/.*-([0-9a-f]+)$/-\1./') | sort) \
            <(ls "$to/deps" | sort) |
            (cd "$from/deps" && xargs -r -d '\n' cp -l -t "$to/deps")

        # Cargo rewrites these small files in place, so they are copied, with
        # mtimes kept. Fingerprints go last: they mark the unit as complete.
        new=$(comm -23 <(echo "$units" | sort) <(ls "$to/.fingerprint" | sort))
        [ -n "$new" ] || continue
        for unit in $new; do
            [ ! -d "$from/build/$unit" ] || echo "$unit"
        done | (cd "$from/build" && xargs -r -d '\n' cp -a -t "$to/build")
        echo "$new" | (cd "$from/.fingerprint" && xargs -r -d '\n' cp -a -t "$to/.fingerprint")
        echo "$profile: $(echo "$new" | wc -l) units -> $dst"
    done
}

mkdir -p "$cache" "$build"
sync "$cache" "$build"
sync "$build" "$cache"
