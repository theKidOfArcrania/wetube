#!/bin/sh
set -eu

# Run from the repository root against a previously built native binary directory.
[ "$#" -eq 1 ] || { echo "Usage: sh docker/test-entrypoint.sh <binary-directory>" >&2; exit 1; }
PATH="$1:$PATH"
export PATH
entrypoint=docker/entrypoint.sh
repository=$(pwd -P)

expect_failure() {
    expected=$1
    shift
    if output=$("$@" 2>&1); then
        echo "Unexpected success: $*" >&2
        exit 1
    fi
    case "$output" in
        *"$expected"*) ;;
        *) printf 'Expected "%s", received: %s\n' "$expected" "$output" >&2; exit 1 ;;
    esac
}

if [ "$(id -u)" -eq 0 ]; then
    expect_failure "non-root UID/GID" sh "$entrypoint" server --version
    echo "Root refusal passed; run as non-root for the remaining checks."
    exit 0
fi

# Help/version need no mounted database; native argument handling stays intact.
sh "$entrypoint" server --version
sh "$entrypoint" worker --version
expect_failure "Unknown role" sh "$entrypoint" invalid serve
expect_failure "subcommand is required" sh "$entrypoint" server
expect_failure "absolute mounted directory" \
    env WETUBE_STATE_DIR=relative sh "$entrypoint" server init
expect_failure "is missing" \
    env WETUBE_STATE_DIR=/proc/self/wetube-missing-root sh "$entrypoint" server init
expect_failure "not writable" \
    env WETUBE_STATE_DIR=/sys sh "$entrypoint" server init
expect_failure "WETUBE_INCOMING_DIR is missing" \
    env WETUBE_STATE_DIR="$repository" WETUBE_INCOMING_DIR=/proc/self/wetube-missing-root \
    sh "$entrypoint" worker run
echo "Entrypoint checks passed without initializing state or starting services."
