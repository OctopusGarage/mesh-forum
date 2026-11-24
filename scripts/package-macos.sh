#!/bin/bash
set -euo pipefail

project="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project"
target="${CARGO_TARGET_DIR:-$project/target}"
bundle="$target/Mesh Forum.app"

cargo build --locked --target-dir "$target" --features desktop --bin mesh-forum-desktop

staging="$(mktemp -d "$target/mesh-forum-package.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
iconset="$staging/AppIcon.iconset"
contents="$staging/Mesh Forum.app/Contents"
mkdir -p "$iconset" "$contents/MacOS" "$contents/Resources"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" assets/mesh-forum-icon.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" assets/mesh-forum-icon.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$contents/Resources/AppIcon.icns"
install -m 755 "$target/debug/mesh-forum-desktop" "$contents/MacOS/mesh-forum-desktop"

version="$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)"
cat > "$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.octopusgarage.mesh-forum</string>
    <key>CFBundleName</key><string>Mesh Forum</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>mesh-forum-desktop</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>15.0</string>
    <key>NSLocalNetworkUsageDescription</key><string>Mesh Forum discovers nearby devices so you can read and share posts over your local network.</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$staging/Mesh Forum.app"
ditto "$staging/Mesh Forum.app" "$bundle"
printf '%s\n' "$bundle"
