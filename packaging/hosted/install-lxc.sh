#!/usr/bin/env bash
# Run inside an existing unprivileged Debian 12/13 Proxmox LXC, from the extracted release.
set -euo pipefail

[[ $(id -u) = 0 ]] || { echo "Run inside the LXC as root" >&2; exit 1; }
[[ $# = 0 || ${1:-} = --no-start ]] || { echo "Usage: $0 [--no-start]" >&2; exit 1; }
# shellcheck source=/dev/null
source /etc/os-release
[[ $ID = debian && ( $VERSION_ID = 12 || $VERSION_ID = 13 ) ]] || {
    echo "This installer supports Debian 12 or 13 guests" >&2
    exit 1
}
[[ $(uname -m) = x86_64 ]] || { echo "This release supports x86_64 guests" >&2; exit 1; }
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
test -x "$root/bin/fcopy-web"
openssl=libssl3
if [[ $VERSION_ID = 13 ]]; then openssl=libssl3t64; fi
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates "$openssl" systemd curl
if ! getent passwd fcopy-web > /dev/null; then
    useradd --system --user-group --home-dir /var/lib/fcopy-web --shell /usr/sbin/nologin fcopy-web
fi
# No recursive chown: data may be a large host bind mount with intentional ownership.
install -d -o fcopy-web -g fcopy-web -m 750 /var/lib/fcopy-web
if [[ ! -d /srv/fcopy ]]; then
    install -d -o fcopy-web -g fcopy-web -m 750 /srv/fcopy
fi
install -m 755 "$root/bin/fcopy-web" /usr/local/bin/fcopy-web
install -m 644 "$root/fcopy-web.service" /etc/systemd/system/fcopy-web.service
if [[ ${1:-} != --no-start ]]; then
    systemctl daemon-reload
    systemctl enable --now fcopy-web.service
    echo "Open https://<LXC-IP>:8443; initial login and certificate fingerprint: journalctl -u fcopy-web"
fi
