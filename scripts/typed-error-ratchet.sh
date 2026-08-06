#!/usr/bin/env bash
# Ratchet on the stringly-error macros (`bail!`, `ensure!`, `anyhow!`, `format_err!`): the
# number of modules opting out of the `clippy::disallowed_macros` ban may only go down.
#
# clippy.toml is the real enforcement (it fails the build on any of them in a module without
# the opt-out). This is the cheap grep CI/pre-commit runs first, and the part clippy cannot
# express: a module may not re-add the opt-out once it has graduated.
set -euo pipefail

readonly ALLOWED=0
readonly ATTR='#![allow(clippy::disallowed_macros)]'

count=$(grep -rlF --include='*.rs' "$ATTR" . | grep -cv '^./third_party/' || true)

if [ "$count" -gt "$ALLOWED" ]; then
    echo "typed-error-ratchet: $count modules opt out of the ban, was $ALLOWED." >&2
    echo "Convert the module to a thiserror enum instead of adding the opt-out." >&2
    exit 1
fi

if [ "$count" -lt "$ALLOWED" ]; then
    echo "typed-error-ratchet: down to $count modules; lower ALLOWED in $0 to $count." >&2
    exit 1
fi
