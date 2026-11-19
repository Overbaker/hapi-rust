#!/bin/sh
printf '%s\n' "$@" > argv
printf '%s\n' "$CODEX_HOME" "${HAPI_SESSION_ID-unset}" > environment
sleep 120 &
echo $! > descendant
wait
