#!/usr/bin/env bash
# Requires an Xvfb server, Mesa software rendering and an installed package or AppImage.
set -euo pipefail

existing_display=false
if [[ ${1:-} = --existing-display ]]; then
    existing_display=true
    shift
    : "${DISPLAY:?An existing X display is required}"
fi
[[ $# -le 1 ]] || { echo "Usage: $0 [--existing-display] [APPIMAGE]" >&2; exit 1; }
work=$(mktemp -d "${TMPDIR:-/tmp}/fcopy-smoke.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
export FCOPY_CONFIG_DIR="$work/config"
export XDG_DATA_HOME="$work/data"
export LIBGL_ALWAYS_SOFTWARE=1
unset WAYLAND_DISPLAY
gui=(fcopy-gui)
cli=(fcopy)
if [[ $# = 1 ]]; then
    image=$(realpath -- "$1")
    (cd "$work" && "$image" --appimage-extract > /dev/null)
    export LD_LIBRARY_PATH="$work/squashfs-root/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    cli=("$work/squashfs-root/usr/bin/fcopy")
    gui=("$image")
    export APPIMAGE_EXTRACT_AND_RUN=1
fi

"${cli[@]}" --help > /dev/null
printf 'FCOPY package smoke test\n' > "$work/source.txt"
"${cli[@]}" copy "$work/source.txt" "$work/destination.txt" \
    --verify readback --on-error cancel
cmp "$work/source.txt" "$work/destination.txt"

# Install/uninstall must also work from an AppImage without saving an extracted path.
"${gui[@]}" --install-integration > "$work/integration.log"
desktop="$XDG_DATA_HOME/applications/io.github.fcopy.desktop"
desktop-file-validate "$desktop"
if [[ $# = 1 ]]; then
    grep -F -- "$image" "$desktop" > /dev/null
    if grep -F 'squashfs-root' "$desktop"; then
        echo "AppImage integration points at a temporary extraction" >&2
        exit 1
    fi
fi
"${gui[@]}" --uninstall-integration > /dev/null

set +e
if $existing_display; then
    timeout --kill-after=5s 10s "${gui[@]}" > "$work/gui.log" 2>&1
else
    xvfb-run -a timeout --kill-after=5s 10s "${gui[@]}" > "$work/gui.log" 2>&1
fi
status=$?
set -e
cat "$work/gui.log"
[[ $status = 124 ]] || { echo "GUI exited unexpectedly: $status" >&2; exit 1; }
[[ -s $FCOPY_CONFIG_DIR/desktop-instance.toml ]] || {
    echo "GUI never reached single-instance initialization" >&2
    exit 1
}
echo "PASS: CLI read-back copy, integration, and headless GUI startup"
