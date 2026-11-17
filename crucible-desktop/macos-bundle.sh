#!/bin/sh
# Assemble the release .app bundle with the blue-shaded icon and the workspace's crucible-lsp
# beside the executable. Run after `cargo build --release` here and `cargo build --release -p
# crucible-lsp` at the repo root. VERSION (default 0.0.0) is the bundle's version. FEATURE names
# a build of an unmerged feature: "Crucible Playbooks (<FEATURE>)" with the green icon, its own
# bundle id and no URL schemes, so it sits beside an installed release and takes none of its
# links.
set -eu
cd "$(dirname "$0")"

VERSION="${VERSION:-0.0.0}"
case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) VERSION=0.0.0 ;;
esac

FEATURE="${FEATURE:-}"
NAME="Crucible Playbooks"
ID=dev.crucible.playbooks
ICON=assets/icon.icns
SCHEMES='    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>dev.crucible.playbooks</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>crucible-playbooks</string>
                <string>crucible</string>
            </array>
        </dict>
    </array>'
if [ -n "$FEATURE" ]; then
    NAME="Crucible Playbooks ($FEATURE)"
    ID="dev.crucible.playbooks.feature.$(printf '%s' "$FEATURE" | tr -c 'A-Za-z0-9-' '-')"
    ICON=assets/icon-feature.icns
    SCHEMES=""
fi

APP="target/release/$NAME.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/crucible-playbooks "$APP/Contents/MacOS/crucible-playbooks"
cp ../target/release/crucible-lsp "$APP/Contents/MacOS/crucible-lsp"
cp "$ICON" "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$NAME</string>
    <key>CFBundleExecutable</key>
    <string>crucible-playbooks</string>
    <key>CFBundleIdentifier</key>
    <string>$ID</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
$SCHEMES
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>
            <string>Playbook pack</string>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>LSHandlerRank</key>
            <string>None</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>public.folder</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist" >/dev/null
codesign --force --deep --sign - "$APP"
echo "$APP"
