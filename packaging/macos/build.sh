#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
[[ $(uname -s) = Darwin ]] || { echo "Build this bundle on macOS" >&2; exit 1; }
architecture=$(uname -m)
case "$architecture" in arm64|x86_64) ;; *) echo "Unsupported Mac architecture: $architecture" >&2; exit 1 ;; esac
version=$(bash packaging/linux/check-version.sh)
mkdir -p target dist
work=$(mktemp -d "$root/target/macos-package.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
app="$work/image/fcopy.app"
contents="$app/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources" "$work/fcopy.iconset"
install -m644 packaging/macos/fcopy.app-template/Contents/Info.plist "$contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$version" "$contents/Info.plist"
plutil -replace CFBundleVersion -string "$version" "$contents/Info.plist"
for binary in fcopy fcopy-gui; do
    install -m755 "target/release/$binary" "$contents/MacOS/$binary"
    [[ $(lipo -archs "$contents/MacOS/$binary") = "$architecture" ]]
    # Release apps must not depend on the CI runner's Homebrew installation.
    if otool -L "$contents/MacOS/$binary" | tail -n +2 | grep -Ev '^[[:space:]]+(/usr/lib/|/System/Library/)'; then
        echo "Non-system dylib dependency in $binary; bundle it before releasing" >&2
        exit 1
    fi
done
rsvg-convert -w 1024 -h 1024 -o "$work/icon.png" packaging/icons/io.github.fcopy.svg
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$work/icon.png" --out "$work/fcopy.iconset/icon_${size}x${size}.png" > /dev/null
    double=$((size * 2))
    sips -z "$double" "$double" "$work/icon.png" --out "$work/fcopy.iconset/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil -c icns "$work/fcopy.iconset" -o "$contents/Resources/fcopy.icns"
codesign --force --sign - "$contents/MacOS/fcopy"
codesign --force --sign - "$contents/MacOS/fcopy-gui"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
plutil -lint "$contents/Info.plist"
ln -s /Applications "$work/image/Applications"
hdiutil create -quiet -volname FCOPY -srcfolder "$work/image" -format UDZO \
    "$root/dist/FCOPY-${version}-macos-${architecture}.dmg"
