#!/bin/sh
# Package the release build as an RPM, crucible-lsp beside the app. Run on Fedora after `cargo
# build --release` here and `cargo build --release -p crucible-lsp` at the repo root, with
# CARGO_TARGET_DIR set if the builds used one. VERSION (default 0.0.0) is the package's version.
# Writes target/linux/crucible-playbooks-<version>-1.<dist>.<arch>.rpm.
set -eu
cd "$(dirname "$0")"

VERSION="${VERSION:-0.0.0}"
case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) VERSION=0.0.0 ;;
esac

app="${CARGO_TARGET_DIR:-target}/release/crucible-playbooks"
lsp="${CARGO_TARGET_DIR:-../target}/release/crucible-lsp"
top="$(pwd)/target/linux/rpmbuild"
rm -rf "$top"
mkdir -p "$top/SOURCES" "$top/SPECS"
cp "$app" "$lsp" linux/dev.crucible.playbooks.desktop "$top/SOURCES/"
cp assets/icon.png "$top/SOURCES/icon.png"
desktop-file-validate linux/dev.crucible.playbooks.desktop
rpmbuild -bb --define "_topdir $top" --define "version $VERSION" linux/crucible-playbooks.spec
find "$top/RPMS" -name '*.rpm' -exec cp {} target/linux/ \;
ls target/linux/*.rpm
