#!/bin/sh
set -eu
umask 077
unset WETUBE_INCOMING_DIR WETUBE_SCRATCH_DIR WETUBE_MEDIA_DIR \
    WETUBE_TMDB_TOKEN_FILE WETUBE_TMDB_ACCEPT_TERMS WETUBE_ALLOW_ANY_HOST

# Pass the exact Compose CMD health probe after the four harness arguments.
[ "$#" -ge 5 ] || {
    echo "Usage: sh docker/test-healthcheck.sh <server> <dist> <disposable-test-parent> <configured-origin> <probe-command> [args...]" >&2
    exit 1
}
server=$(realpath "$1")
static=$(realpath "$2")
root=$(realpath "$3")
configured_origin=$4
shift 4
[ -x "$server" ] && [ -f "$static/index.html" ]
[ -d "$root" ] && [ -w "$root" ] && [ -x "$root" ] || {
    echo "The disposable test parent must be an existing writable/traversable directory." >&2
    exit 1
}

url=http://127.0.0.1:8080/health/ready
if curl --noproxy '*' --silent --output /dev/null --connect-timeout 1 --max-time 1 "$url"; then
    echo "Port 8080 is already serving HTTP; refusing to disturb it." >&2
    exit 1
fi
cd "$root"
workspace=wetube-docker-health-$$
mkdir -m 700 "$workspace"
pid=
stop_server() {
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || :
        wait "$pid" 2>/dev/null || :
        pid=
    fi
}
cleanup() {
    stop_server
    # mkdir above must succeed before this trap owns the unique test directory.
    rm -r -- "$workspace"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

start_server() {
    origin=$1
    case_id=$2
    allow_any_host=${3:-false}
    mkdir "$workspace/$case_id"
    "$server" --state-dir "$workspace/$case_id" init > "$workspace/init.log" 2>&1
    env WETUBE_ALLOW_ANY_HOST="$allow_any_host" timeout 30s "$server" --state-dir "$workspace/$case_id" serve \
        --bind 127.0.0.1:8080 --origin "$origin" --static-dir "$static" \
        > "$workspace/server.log" 2>&1 &
    pid=$!
    attempt=0
    until curl --noproxy '*' --silent --output /dev/null --connect-timeout 1 --max-time 1 "$url"; do
        kill -0 "$pid" 2>/dev/null || { cat "$workspace/server.log" >&2; exit 1; }
        attempt=$((attempt + 1))
        [ "$attempt" -lt 50 ] || { cat "$workspace/server.log" >&2; exit 1; }
        sleep 0.1
    done
    old_status=$(curl --noproxy '*' --silent --output /dev/null --write-out '%{http_code}' --max-time 3 "$url")
    expected=421
    if [ "$allow_any_host" = true ]; then expected=200; fi
    [ "$old_status" = "$expected" ] || {
        echo "Expected loopback probe $expected, received $old_status" >&2
        exit 1
    }
}

start_server "$configured_origin" configured
"$@" > /dev/null
echo "Supplied health probe passed; unqualified loopback probe returned 421."
stop_server

start_server "$configured_origin" host-opt-in true
"$@" > /dev/null
status=$(curl --noproxy '*' --fail --silent --show-error --max-time 3 \
    --output /dev/null --write-out '%{http_code}' \
    --header 'Host: arbitrary.test:9876' "$url")
[ "$status" = 200 ] || { echo "Host opt-in probe returned $status" >&2; exit 1; }
echo "Native WETUBE_ALLOW_ANY_HOST opt-in accepted an arbitrary Host."
stop_server

case_id=0
for origin in \
    http://wetube.test \
    http://wetube.test:80 \
    http://wetube.test:8080 \
    http://wetube.test:8080/ \
    https://wetube.test \
    https://wetube.test:443 \
    https://wetube.test:8443 \
    https://wetube.test:8443/
do
    case_id=$((case_id + 1))
    start_server "$origin" "$case_id"
    authority=${origin#*://}
    authority=${authority%/}
    case "$origin" in
        http://*) authority=${authority%:80} ;;
        https://*) authority=${authority%:443} ;;
    esac
    status=$(curl --noproxy '*' --fail --silent --show-error --max-time 3 \
        --output /dev/null --write-out '%{http_code}' --header "Host: $authority" "$url")
    [ "$status" = 200 ] || { echo "Expected corrected probe 200, received $status" >&2; exit 1; }
    echo "Origin boundary passed: $origin"
    stop_server
done
cleanup
trap - EXIT HUP INT TERM
[ ! -e "$workspace" ]
echo "Health regressions passed; owned listeners stopped and test-state removal verified."
