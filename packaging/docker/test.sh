#!/usr/bin/env bash
# Tests the built image, including a real authenticated restart and paused recovery.
set -euo pipefail

image=${1:-fcopy-web:test}
work=$(mktemp -d "${TMPDIR:-/tmp}/fcopy-container-test.XXXXXXXX")
container="fcopy-test-$(basename "$work")"
cleanup() {
    if docker container inspect "$container" > /dev/null 2>&1; then
        docker logs "$container" 2>&1
        docker rm --force "$container" > /dev/null
    fi
    rm -rf -- "$work"
}
trap cleanup EXIT
mkdir "$work/config" "$work/data"
printf 'initial-test-password\n' > "$work/password"
printf 'persistent recovery test' > "$work/data/source.txt"
docker run --detach --name "$container" --user "$(id -u):$(id -g)" \
    --publish 127.0.0.1::8443 --cap-drop=ALL --security-opt=no-new-privileges \
    --mount "type=bind,src=$work/config,dst=/config" \
    --mount "type=bind,src=$work/data,dst=/data" \
    --mount "type=bind,src=$work/password,dst=/run/bootstrap-password,readonly" \
    "$image" --admin-password-file /run/bootstrap-password --no-torrents > /dev/null
wait_ready() {
    # Docker can assign a different ephemeral host port after a restart.
    port=$(docker port "$container" 8443/tcp | cut -d: -f2)
    url="https://127.0.0.1:$port"
    for ((attempt=0; attempt<100; attempt++)); do
        if curl --insecure --fail --silent --max-time 2 "$url/api/info" > "$work/info"; then return; fi
        [[ $(docker inspect --format '{{.State.Running}}' "$container") = true ]] || return 1
        sleep 0.2
    done
    echo "Console did not become ready" >&2
    return 1
}
login() {
    curl --insecure --fail --silent --show-error -c "$work/cookies" \
        -H 'Content-Type: application/json' \
        --data '{"user":"admin","password":"initial-test-password"}' "$url/api/auth/login" \
        | jq -e '.name == "admin" and .role == "admin"' > /dev/null
}
post() {
    curl --insecure --fail --silent --show-error -b "$work/cookies" \
        -H 'Content-Type: application/json' -H 'x-fcopy-request: 1' \
        --data "$2" "$url$1"
}
wait_ready
cp "$work/info" "$work/first-info"
login
post /api/queue/pause '{}' > /dev/null
post /api/transfers '{"sources":["/data/source.txt"],"dest":"/data/out"}' | jq -e '.id > 0' > /dev/null
docker restart --time 30 "$container" > /dev/null
wait_ready
cmp "$work/first-info" "$work/info"
login
curl --insecure --fail --silent --show-error -b "$work/cookies" "$url/api/recovery" \
    | jq -e '.persistent and (.jobs | length == 1) and .jobs[0].state == "paused"' > /dev/null
test ! -e "$work/data/out/source.txt"
test -s "$work/config/users.json"
if grep -F 'initial-test-password' "$work/config/users.json"; then
    echo "Account file contains plaintext credentials" >&2
    exit 1
fi
echo "PASS: HTTPS login, persistent identity/accounts, graceful restart and paused queue recovery"
