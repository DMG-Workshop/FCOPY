#!/usr/bin/env bash
# Shared package payload. Build the release binaries before staging it.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
destination=${1:?Usage: install.sh ABSOLUTE_STAGING_DIRECTORY}
[[ $destination = /* && $destination != / ]] || {
    echo "Expected an absolute staging directory, not /" >&2
    exit 1
}
cd "$root"

install -Dm755 target/release/fcopy "$destination/usr/bin/fcopy"
install -Dm755 target/release/fcopy-gui "$destination/usr/bin/fcopy-gui"
install -Dm644 packaging/linux/io.github.fcopy.desktop \
    "$destination/usr/share/applications/io.github.fcopy.desktop"
install -Dm644 packaging/linux/io.github.fcopy.metainfo.xml \
    "$destination/usr/share/metainfo/io.github.fcopy.metainfo.xml"
install -Dm755 packaging/linux/fcopy-servicemenu.desktop \
    "$destination/usr/share/kio/servicemenus/fcopy.desktop"
install -Dm644 packaging/linux/fcopy.nemo_action \
    "$destination/usr/share/nemo/actions/fcopy.nemo_action"
install -Dm644 packaging/icons/io.github.fcopy.svg \
    "$destination/usr/share/icons/hicolor/scalable/apps/io.github.fcopy.svg"
install -Dm644 README.md "$destination/usr/share/doc/fcopy/README.md"
