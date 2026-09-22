#!/usr/bin/env bash
set -euo pipefail

arch="$(uname -m)"
case "${arch}" in
    x86_64 | aarch64) ;;
    *) echo "unsupported arch ${arch}" >&2; exit 1 ;;
esac
name="google-workspace-cli-${arch}-unknown-linux-musl.tar.gz"
url="https://github.com/googleworkspace/cli/releases/download/v${PIN_GWS}/${name}"
curl -fsSL -o "/tmp/${name}" "${url}"
curl -fsSL "${url}.sha256" | (cd /tmp && sha256sum -c -)
tar -xzf "/tmp/${name}" -C /tmp ./gws
mv /tmp/gws /usr/local/bin/gws
rm -f "/tmp/${name}"
gws --version

# `gws generate-skills` writes skills/ and docs/ under the cwd; keep the Sheets set.
skills=/usr/local/share/gws/skills
gen="$(mktemp -d)"
(cd "${gen}" && gws generate-skills)
mkdir -p "${skills}"
for s in gws-shared gws-sheets gws-sheets-read gws-sheets-append; do
    cp -r "${gen}/skills/${s}" "${skills}/"
done
rm -rf "${gen}"
test -f "${skills}/gws-sheets-read/SKILL.md"
