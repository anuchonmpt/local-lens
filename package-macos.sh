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

# Package the official client for both Mac architectures and keep its license notices.
kubectl_version=v1.36.6
kubectl_cache="$PWD/target/kubectl-$kubectl_version"
kubectl_release="https://dl.k8s.io/release/$kubectl_version"

fetch_kubernetes_archive() {
    local url="$1" destination="$2" expected="$3"
    if [ -f "$destination" ] && printf '%s  %s\n' "$expected" "$destination" | /usr/bin/shasum -a 256 -c --status; then
        return
    fi
    mkdir -p "$(dirname "$destination")"
    /usr/bin/curl --disable -fsSL --proto '=https' --proto-redir '=https' \
        --connect-timeout 10 --max-time 180 --retry 2 "$url" -o "$stage/kubernetes-download.tar.gz"
    printf '%s  %s\n' "$expected" "$stage/kubernetes-download.tar.gz" | /usr/bin/shasum -a 256 -c
    mv -f "$stage/kubernetes-download.tar.gz" "$destination"
}

stage_kubectl() {
    local arch="$1" checksum="$2" archive="$kubectl_cache/$1/archive.tar.gz"
    fetch_kubernetes_archive "$kubectl_release/kubernetes-client-darwin-$arch.tar.gz" "$archive" "$checksum"
    /usr/bin/tar -xOzf "$archive" kubernetes/client/bin/kubectl > "$stage/kubectl-$arch"
}

stage_kubectl arm64 a47f775aebc96908f5e68332ae481ff5db18a4aae631e1fa6826b45b454da84a
stage_kubectl amd64 5266f7fa1023528fb5812cd8b17152fed0c581e884585f1ed6157ab9ce46a82f
/usr/bin/lipo -create "$stage/kubectl-arm64" "$stage/kubectl-amd64" -output "$app/Contents/MacOS/kubectl"
rm "$stage/kubectl-arm64" "$stage/kubectl-amd64"
chmod 755 "$app/Contents/MacOS/kubectl"
/usr/bin/codesign --force --sign - "$app/Contents/MacOS/kubectl"

fetch_kubernetes_archive "$kubectl_release/kubernetes-src.tar.gz" "$kubectl_cache/source.tar.gz" \
    26e00690ead876ad5d2022c1e63d6b6ec0fbd59727e04f2d87ac0c58a734527c
mkdir -p "$app/Contents/Resources/kubernetes"
/usr/bin/tar -xzf "$kubectl_cache/source.tar.gz" -C "$app/Contents/Resources/kubernetes" LICENSES

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
