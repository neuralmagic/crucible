#!/bin/sh
# Assemble the release .app bundle with the blue-shaded icon and the workspace's crucible-lsp
# beside the executable. Run after `cargo build --release` here and `cargo build --release -p
# crucible-lsp` at the repo root. VERSION (default 0.0.0) is the bundle's version.
set -eu
cd "$(dirname "$0")"

VERSION="${VERSION:-0.0.0}"
case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) VERSION=0.0.0 ;;
esac

APP="target/release/Crucible Playbooks.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/crucible-playbooks "$APP/Contents/MacOS/crucible-playbooks"
cp ../target/release/crucible-lsp "$APP/Contents/MacOS/crucible-lsp"
cp assets/icon.icns "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Crucible Playbooks</string>
    <key>CFBundleDisplayName</key>
    <string>Crucible Playbooks</string>
    <key>CFBundleExecutable</key>
    <string>crucible-playbooks</string>
    <key>CFBundleIdentifier</key>
    <string>dev.crucible.playbooks</string>
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
    <key>CFBundleURLTypes</key>
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
    </array>
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
