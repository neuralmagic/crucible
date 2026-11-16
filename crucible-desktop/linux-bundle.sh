#!/bin/sh
# Package the release build as an RPM. Run on Fedora after build-runtime.sh and `cargo
# build --release` here, with
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
runtime="${CARGO_TARGET_DIR:-../target}/release"
top="$(pwd)/target/linux/rpmbuild"
rm -rf "$top"
mkdir -p "$top/SOURCES" "$top/SPECS"
cp "$app" linux/dev.crucible.playbooks.desktop "$top/SOURCES/"
for binary in crucible-lsp crucible-controller crucible crux; do
    cp "$runtime/$binary" "$top/SOURCES/"
done
cp assets/icon.png "$top/SOURCES/icon.png"
desktop-file-validate linux/dev.crucible.playbooks.desktop
rpmbuild -bb --define "_topdir $top" --define "version $VERSION" linux/crucible-playbooks.spec
find "$top/RPMS" -name '*.rpm' -exec cp {} target/linux/ \;
ls target/linux/*.rpm
