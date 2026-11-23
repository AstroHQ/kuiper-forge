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
  import_rust)
    # the Rust image always runs with --config $KUIPER_DATA_DIR/config.toml: read the same file,
    # when it's there, unless the command line names one
    conf="${KUIPER_DATA_DIR:-/data}/config.toml"
    case " $* " in
      *" --config "* | *" --config="*) ;;
      *) [ -f "$conf" ] && set -- "$@" --config "$conf" ;;
    esac
    exec /app/bin/import_rust "$@"
    ;;
  migrate | server | kuiper_coordinator)
    exec "/app/bin/$cmd" "$@"
    ;;
  *)
    exec "$cmd" "$@"
    ;;
esac
