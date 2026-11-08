#!/bin/sh
# Assemble the release .app bundle with the blue-shaded icon. Run after `cargo build --release`.
set -eu
cd "$(dirname "$0")"

APP="target/release/Crucible Playbooks.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/crucible-playbooks "$APP/Contents/MacOS/crucible-playbooks"
cp assets/icon.icns "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<'EOF'
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
    <string>dev.crucible.playbooks-rune</string>
    <key>CFBundleVersion</key>
    <string>0.0.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.0.0</string>
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
</dict>
</plist>
EOF
echo "$APP"
