#!/usr/bin/env bash
# Run on Linux after cargo build --locked --release -p fc-cli -p fc-gui.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
format=${1:-all}
case "$format" in deb|rpm|appimage|all) ;; *) echo "Usage: $0 [deb|rpm|appimage|all]" >&2; exit 1 ;; esac
[[ $(uname -s) = Linux && $(uname -m) = x86_64 ]] || {
    echo "Release packaging currently supports Linux x86_64 only" >&2
    exit 1
}
package_id=$(cargo pkgid --locked -p fc-gui)
version=${package_id##*#}
version=${version##*@}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "Expected a stable three-component Cargo version, got: $version" >&2
    exit 1
}
mkdir -p target dist
work=$(mktemp -d "$root/target/linux-packaging.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
stage="$work/stage"
bash packaging/linux/install.sh "$stage"
desktop-file-validate "$stage/usr/share/applications/io.github.fcopy.desktop"
appstreamcli validate --no-net "$stage/usr/share/metainfo/io.github.fcopy.metainfo.xml"

if [[ $format = deb || $format = all ]]; then
    mkdir -p "$work/shlibs/debian" "$stage/DEBIAN"
    printf 'Source: fcopy\n\nPackage: fcopy\nArchitecture: any\nDescription: File copying\n' > "$work/shlibs/debian/control"
    dependencies=$(cd "$work/shlibs" && dpkg-shlibdeps -O \
        -e"$stage/usr/bin/fcopy" -e"$stage/usr/bin/fcopy-gui")
    dependencies=$(sed -n 's/^shlibs:Depends=//p' <<< "$dependencies")
    [[ -n $dependencies ]] || { echo "No Debian ELF dependencies found" >&2; exit 1; }
    size=$(du -sk "$stage/usr" | cut -f1)
    sed -e "s/@VERSION@/$version/g" -e "s/@SIZE@/$size/g" \
        -e "s/@DEPENDS@/$dependencies/g" packaging/linux/debian.control.in > "$stage/DEBIAN/control"
    dpkg-deb --root-owner-group --build "$stage" "dist/fcopy_${version}_amd64.deb"
    # DEBIAN is package metadata, not part of the shared payload.
    rm -- "$stage/DEBIAN/control"
    rmdir "$stage/DEBIAN"
fi

if [[ $format = rpm || $format = all ]]; then
    rpmbuild -bb packaging/linux/fcopy.spec \
        --define "_topdir $work/rpm" \
        --define "_build_id_links none" \
        --define "fcopy_version $version" \
        --define "fcopy_stage $stage"
    install -m644 "$work/rpm/RPMS/x86_64/fcopy-${version}-1.x86_64.rpm" dist/
fi

if [[ $format = appimage || $format = all ]]; then
    tool="$work/linuxdeploy-x86_64.AppImage"
    curl --fail --location --retry 3 \
        'https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage' \
        --output "$tool"
    echo "c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d  $tool" | sha256sum --check
    chmod 755 "$tool"
    runtime="$work/runtime-x86_64"
    curl --fail --location --retry 3 \
        'https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64' \
        --output "$runtime"
    echo "2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d  $runtime" | sha256sum --check
    appdir="$work/FCOPY.AppDir"
    mkdir -p "$appdir"
    cp -a "$stage/." "$appdir/"
    # Window-system libraries are loaded at runtime, so ELF scanning alone misses them.
    libraries=()
    for name in libX11.so.6 libXcursor.so.1 libXi.so.6 libXrandr.so.2 \
        libxkbcommon.so.0 libxkbcommon-x11.so.0 libwayland-client.so.0 \
        libwayland-cursor.so.0 libwayland-egl.so.1; do
        path=$(ldconfig -p | awk -v name="$name" '$1 == name && /x86-64/ && !found {print $NF; found=1}')
        [[ -n $path ]] || { echo "Missing AppImage library: $name" >&2; exit 1; }
        libraries+=(--library "$path")
    done
    export APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 LDAI_VERSION="$version"
    export LDAI_OUTPUT="$root/dist/FCOPY-${version}-x86_64.AppImage"
    export LDAI_RUNTIME_FILE="$runtime"
    "$tool" --appdir "$appdir" \
        --executable "$appdir/usr/bin/fcopy-gui" --executable "$appdir/usr/bin/fcopy" \
        --desktop-file "$appdir/usr/share/applications/io.github.fcopy.desktop" \
        --icon-file "$appdir/usr/share/icons/hicolor/scalable/apps/io.github.fcopy.svg" \
        "${libraries[@]}" --output appimage
    [[ -s $LDAI_OUTPUT ]] || { echo "AppImage output was not created" >&2; exit 1; }
    chmod 755 "$LDAI_OUTPUT"
fi
