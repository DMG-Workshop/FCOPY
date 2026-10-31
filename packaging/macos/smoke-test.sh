#!/usr/bin/env bash
set -euo pipefail

image=${1:?Usage: smoke-test.sh IMAGE.dmg}
work=$(mktemp -d "${TMPDIR:-/tmp}/fcopy-macos-test.XXXXXXXX")
mounted=false
pid=
cleanup() {
    if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid"
        wait "$pid" || test $? = 143
    fi
    if $mounted; then hdiutil detach -quiet "$work/mount"; fi
    rm -rf -- "$work"
}
trap cleanup EXIT
mkdir "$work/mount"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$work/mount" "$image"
mounted=true
app="$work/mount/fcopy.app"
codesign --verify --deep --strict "$app"
plutil -lint "$app/Contents/Info.plist"
printf 'FCOPY macOS package test\n' > "$work/source"
"$app/Contents/MacOS/fcopy" copy "$work/source" "$work/copied" --verify readback --on-error cancel
cmp "$work/source" "$work/copied"
export FCOPY_CONFIG_DIR="$work/config"
"$app/Contents/MacOS/fcopy-gui" > "$work/gui.log" 2>&1 &
pid=$!
sleep 10
if ! kill -0 "$pid" 2>/dev/null; then cat "$work/gui.log"; echo "macOS GUI exited unexpectedly" >&2; exit 1; fi
test -s "$FCOPY_CONFIG_DIR/desktop-instance.toml"
echo "PASS: signed bundle, CLI read-back copy and native macOS GUI startup"
