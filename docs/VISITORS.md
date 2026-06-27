# Visitor analytics — architecture breadcrumb

How the gopher-cta visitor-analytics pipeline feeds itself, so future-me doesn't
re-derive it. Breadcrumb, not full docs. (gopher-cta has two surfaces: *serving*
is docs/DEPLOY.md; this is the *analytics* one.)

## Data flow (end to end)
1. **geomyidae** on the RackNerd VPS (`gopher.debene.dev`) writes one access line
   per hit to the bind-mounted host file `/var/log/gopher/geomyidae.log`, rotated
   daily → `geomyidae.log-YYYYMMDD`. Pipe-delimited, only `serving` lines matter:
   `[2026-06-26 12:52:38 +0000|<ip>|<port>|serving] /<selector>`
2. **`scripts/gopher-visitors.py`** parses that log and **enriches** each IP
   offline: ASN/org from a local GeoLite2-ASN `.mmdb`, best-effort reverse DNS, a
   human/bot **verdict**. `--format ndjson` emits one JSON object per hit:
   `ts, ts_ns, ip, selector, rdns, asn, org, kind, verdict, vclass`.
3. **`scripts/visitors-to-loki.py`** reads that NDJSON and **ships** it to Loki
   (`/loki/api/v1/push`). It does *not* parse or enrich — that is step 2.
4. **Loki** stores it under static labels `{job="gopher-cta-visitors",
   host="gopher.debene.dev"}`. Every other field is in the **JSON line body**,
   queried with `| json` (e.g. `{job="gopher-cta-visitors"} | json | vclass="h"`).
   IP/selector are deliberately NOT labels — cardinality.
5. **Grafana** dashboard UID `gopher-cta-visitors` (folder "Gopher-CTA") renders it.

## Where each piece runs
- **VPS** (`gopher.debene.dev`): geomyidae + the log; SSH-readable for the shipper.
- **Shipper** runs in one of two places:
  - **By hand** (workstation): `scripts/visitors-remote.sh` ssh-cats the VPS log
    and runs `gopher-visitors.py | visitors-to-loki.py` locally. Needs `maxminddb`
    + the GeoLite2-ASN DB on that box.
  - **On a timer**: k8s CronJob `gopher-visitors-batch` (ns `observability`, daily
    **09:00 UTC**), image `ghcr.io/felipedbene/gopher-cta-visitors` running
    `visitors-batch.sh` (ssh-cat → enrich → push, all in-cluster), ssh-ing as
    `felipe@gopher.debene.dev` with the key in Secret `gopher-visitors-ssh`.
- **Loki**: ns `observability`, service `loki-gateway:80` (push `http://loki-gateway`).
- **Grafana**: ns `monitoring` (`monitoring-grafana`); the dashboard ConfigMap lives
  in `observability` and is loaded cross-namespace by the `grafana-sc-dashboard`
  sidecar (watches `grafana_dashboard=1` everywhere).

**Scheduler status (2026-06-26):** **live.** The key is authorized on the VPS,
locked to `~/.ssh/gopher-log-reader` (a forced-command wrapper that only `cat`s
the gopher logs), and an end-to-end run pushed live-log hits to Loki
(`pushed 17/17`). Shipping is now **on the timer** (daily 09:00 UTC); manual runs
still work for backfill. Note: a daily run can legitimately push `0 entries` when
that day's dated log holds only operator-IP (`73.211.52.98`) hits — those are
excluded by design (see caveat 2), it is **not** a rotation failure.

## vclass / verdict / kind
- **verdict** (per IP): `HUMAN`, `LIKELY HUMAN`, `BOT/CRAWLER`, `LIKELY BOT`, `UNKNOWN`.
- **kind** (network): `residential`, `datacenter`, `mixed`, `unknown` — from ASN org + rDNS.
- **vclass** collapses verdict for filtering (`gopher-visitors.py::_vclass`):
  `h` = HUMAN/LIKELY HUMAN, `b` = BOT/CRAWLER/LIKELY BOT, `q` = UNKNOWN (the
  catch-all — *not* empty/absent). Verdict itself = kind + request timing
  (human-paced vs bursty) + named-crawler rDNS.

## Run / refresh manually
```sh
# one-shot against the live VPS log, from a workstation:
LOKI_URL=http://LOKI_HOST:3100 scripts/visitors-to-loki.sh           # yesterday's rotated log
REMOTE_LOG=/var/log/gopher/geomyidae.log-YYYYMMDD LOKI_URL=... scripts/visitors-to-loki.sh
# inspect the payload without sending:
scripts/visitors-remote.sh --remote-log <log> --format ndjson | python3 scripts/visitors-to-loki.py --dry-run
```
The CronJob runs the same chain (`visitors-batch.sh`) on the daily timer.

## Caveats (written down on purpose)
1. **The human/bot verdict is a heuristic.** Weakest on long-tail orgs — state-
   telecom / regional-ISP consumer ranges read as residential; some cloud NAT reads
   ambiguous. Don't trust a single-IP verdict on the margin; trust the aggregate.
2. **The operator IP is excluded in the *analyzer*, not upstream.** `gopher-
   visitors.py` drops `SELF_IP_DEFAULT = 73.211.52.98` via the **default**
   `--exclude-ip`. The VPS still logs it and Loki would store it — only that default
   keeps it out. `--exclude-ip` *replaces* the default (doesn't append), so any
   caller passing its own `--exclude-ip` silently re-includes the operator IP unless
   it re-lists `73.211.52.98`. `visitors-batch.sh` passes none today (relies on the default).

## Deploy gotchas (learned 2026-06-26 — so we never re-debug the CronJob)
The image+secret dance that made the first CronJob runs fail, and the fixes:
- **The image MUST be multi-arch.** The cluster is mixed — amd64 nodes
  (`intel5/intel9/ultra2/zima`) **plus `orion` (arm64)** — and a CronJob pod can
  land on any of them. An amd64-only image → `ImagePullBackOff` on orion. Build and
  push both arches (single `--load` won't do multi-arch):
  ```sh
  docker buildx build --platform linux/amd64,linux/arm64 \
    -f deploy/Dockerfile.visitors \
    -t ghcr.io/felipedbene/gopher-cta-visitors:latest --push .
  ```
- **The GHCR package is PRIVATE** (the main `gopher-cta` image is public; this one
  was left private). So the CronJob needs `imagePullSecrets: [ghcr-pull]` (already
  wired in `deploy/visitors-cronjob.yaml`). Create the secret from a token with
  `read:packages`:
  ```sh
  kubectl -n observability create secret docker-registry ghcr-pull \
    --docker-server=ghcr.io --docker-username=felipedbene \
    --docker-password="$(gh auth token)"     # token needs read:packages
  ```
  Note: a `gh` OAuth token works but rotates on re-auth — for a long-lived secret,
  prefer a dedicated `read:packages` PAT. (Or just make the package public and drop
  the secret + the `imagePullSecrets` block.)
- **The SSH key must be authorized on the VPS.** Secret `gopher-visitors-ssh` holds
  the *private* key (`SSH_KEY=/keys/id_ed25519`); its *public* half must be in
  `felipe@gopher.debene.dev:~/.ssh/authorized_keys`, ideally restricted:
  `command="cat /var/log/gopher/geomyidae.log-*",no-pty,no-port-forwarding <key>`.
  Missing → `Permission denied (publickey)` — the batch's only real failure mode
  once the image/secret are sorted.

Verify a run: `kubectl -n observability create job --from=cronjob/gopher-visitors-batch viz-test`
then `kubectl -n observability logs -f job/viz-test`.
