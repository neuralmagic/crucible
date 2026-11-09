#!/bin/sh
# Install Crucible Playbooks, the desktop app, from a GitHub release: the newest, or the version
# given (`sh install-desktop.sh 0.6.1`). On an Apple silicon Mac it unpacks the app into
# $APPS (default /Applications); on Fedora it installs the RPM with dnf.
#
#   curl -fsSL https://raw.githubusercontent.com/neuralmagic/crucible/main/scripts/install-desktop.sh | sh
set -eu

repo=neuralmagic/crucible
app="Crucible Playbooks.app"

die() {
    echo "install-desktop: $*" >&2
    exit 1
}

if [ -n "${1:-}" ]; then
    tag="v${1#v}"
else
    tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
        sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$tag" ] || die "could not read the newest release of $repo"
fi
version=${tag#v}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Download one asset of the release and check it against the .sha256 published beside it.
fetch() {
    url="https://github.com/$repo/releases/download/$tag/$1"
    curl -fL --progress-bar -o "$tmp/$1" "$url" ||
        die "$tag has no $1 (see https://github.com/$repo/releases/tag/$tag)"
    curl -fsSL -o "$tmp/$1.sha256" "$url.sha256" || die "$tag has no $1.sha256"
    (cd "$tmp" && verify "$1.sha256" >/dev/null) || die "$1 does not match its checksum"
}

case "$(uname -s)" in
Darwin)
    [ "$(uname -m)" = arm64 ] || die "the macOS app is built for Apple silicon only"
    verify() { shasum -a 256 -c "$1"; }
    zip="Crucible-Playbooks-$tag-aarch64-apple-darwin.zip"
    fetch "$zip"
    apps="${APPS:-/Applications}"
    osascript -e 'quit app "Crucible Playbooks"' >/dev/null 2>&1 || true
    rm -rf "${apps:?}/$app"
    ditto -x -k "$tmp/$zip" "$apps"
    xattr -dr com.apple.quarantine "$apps/$app" 2>/dev/null || true
    echo "Installed Crucible Playbooks $version in $apps. Open it with: open '$apps/$app'"
    ;;
Linux)
    # shellcheck source=/dev/null
    [ -r /etc/os-release ] && . /etc/os-release
    [ "${ID:-}" = fedora ] || die "the Linux build is an RPM for Fedora; this is ${ID:-an unknown distribution}"
    arch=$(uname -m)
    case "$arch" in
    x86_64 | aarch64) ;;
    *) die "there is no RPM for $arch" ;;
    esac
    verify() { sha256sum -c "$1"; }
    rpm="crucible-playbooks-$version-1.fc45.$arch.rpm"
    fetch "$rpm"
    if [ "$(id -u)" -eq 0 ]; then
        dnf install -y "$tmp/$rpm"
    else
        sudo dnf install -y "$tmp/$rpm"
    fi
    echo "Installed Crucible Playbooks $version. Open it from the app grid, or run crucible-playbooks."
    ;;
*)
    die "no desktop build for $(uname -s); see https://github.com/$repo/releases"
    ;;
esac
