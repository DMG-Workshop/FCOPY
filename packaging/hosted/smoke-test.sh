#!/usr/bin/env bash
# Inside the disposable Debian CI guest, after install-lxc.sh --no-start.
set -euo pipefail

work=$(mktemp -d)
pid=
cleanup() {
    if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid"
        wait "$pid" || test $? = 143
    fi
    cat "$work/server.log"
    rm -rf -- "$work"
}
trap cleanup EXIT
chmod 755 "$work"
printf 'native-package-test-password\n' > "$work/password"
runuser -u fcopy-web -- /usr/local/bin/fcopy-web --config /var/lib/fcopy-web \
    --root /srv/fcopy --listen 127.0.0.1:8443 --no-torrents \
    --admin-password-file "$work/password" > "$work/server.log" 2>&1 &
pid=$!
for ((attempt=0; attempt<100; attempt++)); do
    if curl --insecure --fail --silent --max-time 2 https://127.0.0.1:8443/api/info > "$work/info"; then break; fi
    kill -0 "$pid"
    sleep 0.1
done
grep -q '"scheme":"https"' "$work/info"
test -s /var/lib/fcopy-web/users.json
test "$(stat -c '%U' /var/lib/fcopy-web/users.json)" = fcopy-web
echo "PASS: native hosted package starts as the non-root LXC service account"
