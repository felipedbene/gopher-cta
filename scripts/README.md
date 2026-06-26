# scripts/

Standalone, **read-only** operator tools. Nothing here is part of the serving
path: these scripts never import from `src/`, never write into `./public`, never
touch the running containers, and stand up no database, cron job, scheduler, or
server. They recompute from their inputs on every run and persist nothing
derived. Run them by hand over SSH.

## `gopher-visitors.py` — who is visiting the gopher server?

Enriches the persisted geomyidae access log
(`/var/log/gopher/geomyidae.log`, the bind-mounted flat file — see
[`../docs/DEPLOY.md`](../docs/DEPLOY.md) "Logs") to answer "who's hitting this?".

For each connecting IP it:

1. parses the log, keeping only `serving` lines, and drops excluded IPs
   (`--exclude-ip`, default once for felipe's own `73.211.52.98` so testing /
   kiosk traffic doesn't pollute the report);
2. enriches the IP **offline** — ASN/org from a local MaxMind GeoLite2-ASN
   `.mmdb` (downloaded once, never queried live) + best-effort reverse DNS
   (cached per run);
3. stitches each IP's ordered selector trail with inter-hit timing;
4. classifies it **human vs bot/crawler** with a transparent heuristic
   (residential ASN + human-paced trail → human; datacenter ASN / crawler rDNS
   + bursty trail → bot) and prints the reasoning, never hides it;
5. prints a ranked plaintext report to stdout (`--out FILE` also writes a copy).

### Run

```sh
# default: /var/log/gopher/geomyidae.log, excludes 73.211.52.98
python3 scripts/gopher-visitors.py

# point at a file, write a copy, drop extra IPs
python3 scripts/gopher-visitors.py --log /var/log/gopher/geomyidae.log-20260626 \
    --exclude-ip 73.211.52.98 --out /tmp/visitors.txt

# offline demo against the bundled sample (no VPS needed)
python3 scripts/gopher-visitors.py --log scripts/sample-access.log
```

Process **yesterday's** rotated file (`geomyidae.log-YYYYMMDD`) for a clean
day-boundary batch — not the live `geomyidae.log`, which is being written to.

Flags: `--log` (`-` reads stdin), `--source-label TEXT`, `--exclude-ip`
(repeatable; `--exclude-ip ''` excludes nothing), `--asn-db PATH`,
`--download-asn`, `--license-key`, `--no-rdns`, `--max-trail N`, `--out FILE`.

### Temporal distribution (post-release impact)

`--timeline` appends an ASCII histogram of served hits over time, split
human/bot/unknown per bucket (UTC). `--bucket` sets the bin (`day`, `hour`, or
`N` / `Nm` minutes; default `hour`). `--release '<ts>'` marks a release moment
and prints a before/after summary (hits, distinct IPs, humans, bots) — handy for
gauging what a launch actually drove. The release timestamp is UTC; convert from
local first (e.g. Chicago CDT 11:37 → `'2026-06-25 16:37'`).

```sh
scripts/visitors-remote.sh --remote-log /var/log/gopher/geomyidae.log-20260625-23 \
    --release '2026-06-25 16:37' --out ~/post-release.txt
scripts/visitors-remote.sh --bucket 15m --timeline      # finer-grained spike view
```

### Run it against the live VPS — `visitors-remote.sh`

One-shot wrapper: SSH to the gopher VPS, `cat` the remote geomyidae log, and pipe
it into `gopher-visitors.py` running **locally** (so the ASN DB + reverse DNS
stay on your machine). READ-ONLY on the server, single run, no daemon.

```sh
scripts/visitors-remote.sh                                    # live log, default host
scripts/visitors-remote.sh --remote-log /var/log/gopher/geomyidae.log-20260626
scripts/visitors-remote.sh --out ~/visitors.txt --no-rdns     # extra flags pass through
GOPHER_SSH=felipe@192.210.238.140 scripts/visitors-remote.sh  # override host (or ssh alias)
```

Host defaults to `$GOPHER_SSH` (else `felipe@gopher.debene.dev`); remote log to
`/var/log/gopher/geomyidae.log`. Any flag it doesn't recognise is forwarded
verbatim to the analyzer. The live `geomyidae.log` is whatever has accumulated
since the last rotation — point `--remote-log` at a dated
`geomyidae.log-YYYYMMDD` for a full day's window.

### ASN database (offline, one-time)

ASN/org enrichment reads a local **GeoLite2-ASN** `.mmdb` — no live API, no key
in the hot loop. Reading it needs the `maxminddb` package:

```sh
pip install maxminddb     # or: apt-get install python3-maxminddb
```

Get the DB once (free MaxMind account → license key):

```sh
export MAXMIND_LICENSE_KEY=xxxxxxxx
python3 scripts/gopher-visitors.py --download-asn   # caches to ~/.cache/gopher-cta/
```

Or pass an existing file with `--asn-db PATH`, or set `$GEOLITE2_ASN_DB`. The
`.mmdb` is **gitignored** — it's a downloaded artifact, not source.

**Without a DB the script still runs** — it falls back to reverse-DNS + timing
only (which already classifies most crawlers correctly) and says so in the
header. `--no-rdns` skips reverse DNS entirely for a fully offline, timing-only
run.
