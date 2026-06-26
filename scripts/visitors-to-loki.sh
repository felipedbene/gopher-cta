#!/usr/bin/env bash
# visitors-to-loki.sh — daily batch: pull a rotated geomyidae log from the VPS,
# enrich it locally (ASN + reverse DNS + human/bot verdict), and push the result
# to Grafana Loki. READ-ONLY on the server; the only writes are the Loki POSTs.
#
# Runs the enrichment LOCALLY, so it needs maxminddb + the GeoLite2-ASN.mmdb on
# whatever host runs it (already set up on the Mac). Wire it to cron/launchd
# yourself for a true daily cadence.
#
#   LOKI_URL=http://10.0.0.5:3100 scripts/visitors-to-loki.sh           # yesterday's log
#   REMOTE_LOG=/var/log/gopher/geomyidae.log-20260625-23 \
#       LOKI_URL=... scripts/visitors-to-loki.sh                        # a specific file
#   scripts/visitors-to-loki.sh --dry-run                              # inspect, send nothing
#
# Env: GOPHER_SSH (default felipe@gopher.debene.dev), REMOTE_LOG (default: the
# VPS's yesterday-dated rotated file), plus the LOKI_* vars read by
# visitors-to-loki.py. Any extra CLI args pass through to that pusher.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOST="${GOPHER_SSH:-felipe@gopher.debene.dev}"

# Yesterday in UTC, BSD (macOS) or GNU (Linux) date — for the dateext filename.
yday="$(date -u -v-1d +%Y%m%d 2>/dev/null || date -u -d 'yesterday' +%Y%m%d)"
REMOTE_LOG="${REMOTE_LOG:-/var/log/gopher/geomyidae.log-${yday}}"

echo "batch: ${HOST}:${REMOTE_LOG} -> Loki" >&2
ssh "$HOST" "cat -- '$REMOTE_LOG'" \
  | python3 "$DIR/gopher-visitors.py" --log - --format ndjson \
  | python3 "$DIR/visitors-to-loki.py" "$@"
