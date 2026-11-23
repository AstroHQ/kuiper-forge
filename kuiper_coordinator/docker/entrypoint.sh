#!/bin/sh
# start (default): migrate, then serve. Any other first word is a release script in /app/bin
# (migrate, server, import_rust, kuiper_coordinator) or, failing that, a command to run.
set -e

cmd="${1:-start}"
[ $# -gt 0 ] && shift

case "$cmd" in
  start)
    /app/bin/migrate
    exec /app/bin/server
    ;;
  migrate | server | import_rust | kuiper_coordinator)
    exec "/app/bin/$cmd" "$@"
    ;;
  *)
    exec "$cmd" "$@"
    ;;
esac
