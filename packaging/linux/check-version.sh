#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$root"
package_id=$(cargo pkgid --locked -p fc-gui)
version=${package_id##*#}
version=${version##*@}
recipe_version=$(sed -n 's/^pkgver=//p' packaging/arch/PKGBUILD)
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && $version = "$recipe_version" ]] || {
    echo "Cargo version ($version) and Arch pkgver ($recipe_version) must match and be stable X.Y.Z versions" >&2
    exit 1
}
if [[ ${GITHUB_REF_TYPE:-} = tag && ${GITHUB_REF_NAME:-} != "v$version" ]]; then
    echo "Release tag (${GITHUB_REF_NAME:-}) must equal v$version" >&2
    exit 1
fi
printf '%s\n' "$version"
