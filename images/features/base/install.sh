#!/usr/bin/env bash
set -euo pipefail

arch="$(uname -m)"
case "${arch}" in
    x86_64) gh_arch=amd64 ;;
    aarch64) gh_arch=arm64 ;;
    *) echo "unsupported arch ${arch}" >&2; exit 1 ;;
esac

python3.12 -m pip install --no-cache-dir "uv==${PIN_UV}"

curl -fsSL -o /tmp/gh.tar.gz \
    "https://github.com/cli/cli/releases/download/v${PIN_GH}/gh_${PIN_GH}_linux_${gh_arch}.tar.gz"
tar -xzf /tmp/gh.tar.gz --strip-components=2 -C /usr/local/bin \
    "gh_${PIN_GH}_linux_${gh_arch}/bin/gh"
rm -f /tmp/gh.tar.gz

curl -fsSL -o /tmp/binstall.tgz \
    "https://github.com/cargo-bins/cargo-binstall/releases/download/v${PIN_CARGO_BINSTALL}/cargo-binstall-${arch}-unknown-linux-gnu.tgz"
tar -xzf /tmp/binstall.tgz -C /usr/local/bin cargo-binstall
rm -f /tmp/binstall.tgz

curl -fsSL -o /tmp/ujira.tar.gz \
    "https://github.com/wseaton/ujira/releases/download/v${PIN_UJIRA}/ujira-${arch}-unknown-linux-gnu.tar.gz"
tar -xzf /tmp/ujira.tar.gz -C /usr/local/bin ujira
rm -f /tmp/ujira.tar.gz

git --version
node --version
uv --version
gh --version
ujira --version
