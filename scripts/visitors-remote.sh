#!/usr/bin/env bash
# visitors-remote.sh — one-shot: SSH to the gopher VPS, stream the geomyidae
# access log over the wire, and run the local visitor analysis on it.
#
# READ-ONLY end to end: it only `cat`s the remote log (no writes on the server),
# pipes it straight into scripts/gopher-visitors.py here, and persists nothing
# unless you pass --out. Single run, no loop, no daemon.
#
# Usage:
#   scripts/visitors-remote.sh                                  # default host + live log
#   scripts/visitors-remote.sh --out ~/visitors.txt             # extra flags pass through
#   scripts/visitors-remote.sh --remote-log /var/log/gopher/geomyidae.log-20260626
#   scripts/visitors-remote.sh --host felipe@192.210.238.140
#   GOPHER_SSH=gopher-vps scripts/visitors-remote.sh            # or use an ssh config alias
#
# Any flag this script doesn't recognise (--out, --no-rdns, --exclude-ip,
# --asn-db, --max-trail, ...) is forwarded verbatim to gopher-visitors.py.
set -euo pipefail

HOST="${GOPHER_SSH:-felipe@gopher.debene.dev}"
REMOTE_LOG="/var/log/gopher/geomyidae.log"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

pass=()
while [ $# -gt 0 ]; do
  case "$1" in
    --host)        HOST="$2"; shift 2 ;;
    --host=*)      HOST="${1#*=}"; shift ;;
    --remote-log)  REMOTE_LOG="$2"; shift 2 ;;
    --remote-log=*) REMOTE_LOG="${1#*=}"; shift ;;
    -h|--help)
      sed -n '2,18p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) pass+=("$1"); shift ;;
  esac
done

echo "connecting to $HOST — reading $REMOTE_LOG ..." >&2
# `cat` the remote log; the analysis (ASN DB + reverse DNS) runs locally on stdin.
ssh "$HOST" "cat -- '$REMOTE_LOG'" \
  | python3 "$DIR/gopher-visitors.py" \
      --log - --source-label "$HOST:$REMOTE_LOG" "${pass[@]+"${pass[@]}"}"
