#!/bin/sh
set -eu
cd "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'Packaging the macOS disk image requires macOS.' >&2
    exit 1
fi

version="$(sed -n 's/^version = "\([0-9][0-9.]*\)"$/\1/p' Cargo.toml)"
dist="$PWD/target/dist"
mkdir -p "$dist"
stage="$(mktemp -d "$PWD/target/dmg.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
trap 'exit 1' HUP INT TERM

app="$stage/Cluster Observatory.app"
iconset="$stage/AppIcon.iconset"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$iconset"
for size in 16 32 128 256 512; do
    retina=$((size * 2))
    /usr/bin/sips -z "$size" "$size" assets/app-icon.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    /usr/bin/sips -z "$retina" "$retina" assets/app-icon.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
/usr/bin/iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -r "$iconset"

./build.sh --release --locked --target-dir "$PWD/target" --target aarch64-apple-darwin --target x86_64-apple-darwin
/usr/bin/lipo -create \
    target/aarch64-apple-darwin/release/cluster-observatory \
    target/x86_64-apple-darwin/release/cluster-observatory \
    -output "$app/Contents/MacOS/cluster-observatory"
chmod 755 "$app/Contents/MacOS/cluster-observatory"

cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Cluster Observatory</string>
    <key>CFBundleIdentifier</key><string>local.lenser.cluster-observatory</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleExecutable</key><string>cluster-observatory</string>
    <key>CFBundleIconFile</key><string>AppIcon.icns</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

/usr/bin/codesign --force --sign - "$app"
/usr/bin/codesign --verify --deep --strict "$app"
ln -s /Applications "$stage/Applications"
dmg="$dist/Cluster-Observatory-$version-macOS.dmg"
/usr/bin/hdiutil create -ov -format UDZO -volname "Cluster Observatory" -srcfolder "$stage" "$dmg"
/usr/bin/hdiutil verify "$dmg"
printf '%s\n' "$dmg"
