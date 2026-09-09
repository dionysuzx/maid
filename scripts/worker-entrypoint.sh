#!/bin/sh
set -eu

credential_source=/run/maid/codex-auth/auth.json

if [ -f "$credential_source" ]; then
    umask 077
    cp "$credential_source" "$CODEX_HOME/auth.json"
fi

exec codex "$@"
