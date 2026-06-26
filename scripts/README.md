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

Flags: `--log`, `--exclude-ip` (repeatable; `--exclude-ip ''` excludes nothing),
`--asn-db PATH`, `--download-asn`, `--license-key`, `--no-rdns`, `--max-trail N`,
`--out FILE`.

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
