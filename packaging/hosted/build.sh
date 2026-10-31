#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
[[ $(uname -s) = Linux && $(uname -m) = x86_64 ]] || {
    echo "Native hosted releases currently support Linux x86_64" >&2
    exit 1
}
version=$(bash packaging/linux/check-version.sh)
mkdir -p target dist
work=$(mktemp -d "$root/target/hosted-package.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
stage="$work/fcopy-web-$version"
install -Dm755 target/release/fcopy-web "$stage/bin/fcopy-web"
install -m644 packaging/hosted/fcopy-web.service "$stage/"
install -m755 packaging/hosted/install-lxc.sh "$stage/"
install -m644 packaging/hosted/README.md "$stage/"
tar -C "$work" -czf "dist/fcopy-web-${version}-linux-x86_64.tar.gz" "fcopy-web-$version"
install -m644 packaging/unraid/fcopy.xml "dist/fcopy-unraid.xml"
