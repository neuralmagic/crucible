#!/bin/sh
# Check out anomalyco/rune at the pinned revision into ../vendor/rune and apply patches/rune/*.patch,
# so the app builds against upstream plus the changes it carries rather than a fork. A no-op when
# the checkout already holds this revision with these patches.
#
# To carry a new change: edit ../vendor/rune, then
#   git -C ../vendor/rune diff > patches/rune/NNNN-what-it-does.patch
# To move to a new upstream revision: change REV, run this, and fix any patch that no longer applies.
set -eu
cd "$(dirname "$0")"

REV=4ca683561a100e16b1ce32f3576f4ade61bd2924
DIR=../vendor/rune

stamp="$REV $(cat patches/rune/*.patch 2>/dev/null | { shasum -a 256 2>/dev/null || sha256sum; } | cut -c1-16)"
if [ -f "$DIR/.patched" ] && [ "$(cat "$DIR/.patched")" = "$stamp" ]; then
    exit 0
fi

if [ ! -d "$DIR/.git" ]; then
    git clone --quiet --filter=blob:none --no-checkout https://github.com/anomalyco/rune "$DIR"
fi
git -C "$DIR" cat-file -e "$REV^{commit}" 2>/dev/null || git -C "$DIR" fetch --quiet origin "$REV"
git -C "$DIR" checkout --quiet --force --detach "$REV"
git -C "$DIR" reset --quiet --hard "$REV"
git -C "$DIR" clean --quiet -fd
for patch in patches/rune/*.patch; do
    [ -e "$patch" ] || continue
    if ! git -C "$DIR" apply --whitespace=nowarn "$PWD/$patch"; then
        echo "rune-patches: $patch does not apply to $REV" >&2
        exit 1
    fi
done
echo "$stamp" > "$DIR/.patched"
