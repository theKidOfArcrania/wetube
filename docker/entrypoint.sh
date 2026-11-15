#!/bin/sh
set -eu
umask 077

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

require_root() {
    name=$1
    path=$2
    access=$3
    case "$path" in
        /*) ;;
        *) fail "$name must be an absolute mounted directory" ;;
    esac
    [ -d "$path" ] || fail "$name is missing: $path (provision the bind mount first)"
    [ -r "$path" ] && [ -x "$path" ] || fail "$name is not readable/traversable: $path"
    if [ "$access" = write ]; then
        [ -w "$path" ] || fail "$name is not writable by the configured UID/GID: $path"
    fi
}

[ "$(id -u)" -ne 0 ] || fail "Run WeTube as a non-root UID/GID with dataset ACL access"
[ "$#" -gt 0 ] || fail "Select server or worker"
role=$1
shift
case "$role" in
    server|worker) ;;
    *) fail "Unknown role: $role (use server or worker)" ;;
esac
[ "$#" -gt 0 ] || fail "A $role subcommand is required"
case "$1" in
    --help|-h|--version|-V|doctor) ;;
    *)
        require_root WETUBE_STATE_DIR "${WETUBE_STATE_DIR:?}" write
        case "$role:$1" in
            server:serve|worker:run)
                require_root WETUBE_INCOMING_DIR "${WETUBE_INCOMING_DIR:?}" read
                require_root WETUBE_SCRATCH_DIR "${WETUBE_SCRATCH_DIR:?}" write
                if [ "$role" = worker ]; then
                    require_root WETUBE_MEDIA_DIR "${WETUBE_MEDIA_DIR:?}" write
                else
                    require_root WETUBE_MEDIA_DIR "${WETUBE_MEDIA_DIR:?}" read
                    [ -n "${WETUBE_ORIGIN:-}" ] || fail "WETUBE_ORIGIN must match the browser's exact origin"
                fi
                ;;
        esac
        ;;
esac
exec "wetube-$role" "$@"
