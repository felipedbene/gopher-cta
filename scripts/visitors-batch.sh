#!/usr/bin/env sh
# visitors-batch.sh — container entrypoint for the in-cluster daily batch.
#
#   ssh-cat the VPS's rotated geomyidae log  ->  gopher-visitors.py --format ndjson
#   (ASN via the baked GeoLite2-ASN.mmdb + reverse DNS + human/bot verdict)
#   ->  visitors-to-loki.py  ->  http://loki-gateway  (in-cluster, nothing exposed).
#
# Tolerant by design: a missing or empty dated log exits 0 — "no rotation / no
# traffic that day" is not a failure and must not page anyone.
#
# Config (env):
#   GOPHER_SSH    user@host of the VPS            (required)
#   LOKI_URL      e.g. http://loki-gateway        (required)
#   SSH_KEY       private key path                (default /keys/id_ed25519)
#   KNOWN_HOSTS   writable known_hosts path       (default /tmp/known_hosts)
#   REMOTE_LOG    log to read    (default: the VPS's yesterday-dated rotated file)
#   DAYS_AGO      how many days back for the default REMOTE_LOG (default 1)
#   NO_RDNS=1     skip reverse DNS (if pod egress can't do PTR lookups)
# Extra args are forwarded to visitors-to-loki.py (e.g. --extra-label env=prod).
set -eu

HOST="${GOPHER_SSH:?GOPHER_SSH required (user@host)}"
: "${LOKI_URL:?LOKI_URL required (e.g. http://loki-gateway)}"
KEYSRC="${SSH_KEY:-/keys/id_ed25519}"
KNOWN_HOSTS="${KNOWN_HOSTS:-/tmp/known_hosts}"

# ssh refuses keys with group/world-readable perms, and the mounted Secret is
# owned by root with a fixed mode — so copy it to a private 0600 file we own.
# (mktemp creates 0600; cp keeps the destination mode.)
KEY="$(mktemp)"
cp "$KEYSRC" "$KEY"
chmod 600 "$KEY"
DAYS_AGO="${DAYS_AGO:-1}"
REMOTE_LOG="${REMOTE_LOG:-/var/log/gopher/geomyidae.log-$(date -u -d "${DAYS_AGO} days ago" +%Y%m%d)}"

SSH="ssh -i ${KEY} -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=${KNOWN_HOSTS} -o ConnectTimeout=10"
RDNS_FLAG=""
[ "${NO_RDNS:-0}" = "1" ] && RDNS_FLAG="--no-rdns"

echo "[visitors-batch] ${HOST}:${REMOTE_LOG} -> ${LOKI_URL}" >&2

tmp="$(mktemp)"
err="$(mktemp)"
trap 'rm -f "$tmp" "$err" "$KEY"' EXIT

# ssh exit 255 = ssh-level failure (connection/auth) -> FAIL LOUDLY (don't silently
# ship nothing forever). A non-zero from the remote `cat` (e.g. 1) = the dated file
# isn't there yet -> tolerate (no rotation/traffic that day is not an error).
# `|| rc=$?` keeps `set -e` from aborting here before we inspect the code.
rc=0
$SSH "$HOST" "cat -- '$REMOTE_LOG'" >"$tmp" 2>"$err" || rc=$?
if [ "$rc" -eq 255 ]; then
  echo "[visitors-batch] SSH failure (rc=255) reaching ${HOST}: $(tr '\n' ' ' <"$err")" >&2
  exit 1
fi
if [ "$rc" -ne 0 ]; then
  echo "[visitors-batch] ${REMOTE_LOG} not readable (cat rc=${rc}) — likely not rotated yet; exit 0" >&2
  exit 0
fi
if [ ! -s "$tmp" ]; then
  echo "[visitors-batch] ${REMOTE_LOG} is empty — nothing to ship" >&2
  exit 0
fi

# shellcheck disable=SC2086  # RDNS_FLAG is intentionally word-split (empty or --no-rdns)
python3 /app/gopher-visitors.py --log "$tmp" --format ndjson $RDNS_FLAG \
  | python3 /app/visitors-to-loki.py "$@"
