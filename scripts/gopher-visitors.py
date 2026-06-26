#!/usr/bin/env python3
"""gopher-visitors.py — who is visiting the gopher server?

READ-ONLY analysis of the geomyidae access log. It enriches each connecting IP
with ASN/org (offline MaxMind lookup) + reverse DNS, stitches each IP's request
trail with timing, and classifies it human vs bot/crawler with a transparent
heuristic.

It is deliberately isolated from the serving path:
  - it never imports from src/, never writes into ./public, never touches the
    running containers;
  - it makes NO live per-IP API calls in the hot loop (ASN is a local DB lookup;
    the only network is best-effort reverse DNS, which is cached per run);
  - it persists nothing derived — the log is the source of truth and every run
    recomputes from it. (--out only writes a copy of the human-readable report.)

Usage:
    python3 scripts/gopher-visitors.py                       # default log + exclusions
    python3 scripts/gopher-visitors.py --log /var/log/gopher/geomyidae.log
    python3 scripts/gopher-visitors.py --exclude-ip 73.211.52.98 --exclude-ip 1.2.3.4
    python3 scripts/gopher-visitors.py --no-rdns             # skip reverse DNS (faster)
    python3 scripts/gopher-visitors.py --out report.txt      # also write the report

ASN database (offline, downloaded ONCE — never queried live):
    # needs a free MaxMind license key in $MAXMIND_LICENSE_KEY
    python3 scripts/gopher-visitors.py --download-asn
    # or point at an existing file / set $GEOLITE2_ASN_DB
    python3 scripts/gopher-visitors.py --asn-db ~/GeoLite2-ASN.mmdb
Reading the .mmdb needs the `maxminddb` package (pip install maxminddb). Without
a DB the script still runs — it just falls back to reverse-DNS + timing only and
says so.
"""
from __future__ import annotations

import argparse
import contextlib
import os
import re
import socket
import statistics
import sys
import tarfile
import tempfile
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone

LOG_DEFAULT = "/var/log/gopher/geomyidae.log"
SELF_IP_DEFAULT = "73.211.52.98"  # felipe's own testing/kiosk IP — dropped by default

# geomyidae access line: "[2026-06-26 12:52:38 +0000|73.211.52.98|60516|serving] /map.ansi"
LINE_RE = re.compile(
    r"^\[(?P<ts>[^|]+)\|(?P<ip>[^|]+)\|(?P<port>[^|]+)\|(?P<status>[^\]]+)\]\s*(?P<sel>.*?)\s*$"
)

# --- classification vocabularies (lowercased substring match) ---------------
# ASN org names that mean "this is a hosting/cloud/datacenter network".
DC_KEYWORDS = (
    "amazon", "aws", "google", "microsoft", "azure", "ovh", "hetzner",
    "digitalocean", "digital ocean", "linode", "akamai", "cloudflare", "fastly",
    "vultr", "choopa", "leaseweb", "contabo", "oracle", "alibaba", "tencent",
    "scaleway", "m247", "datacamp", "gcore", "g-core", "hostwinds", "ramnode",
    "colocrossing", "colo", "hosting", "datacenter", "data center", "dedicated",
    "vps", "cloud", "ionos", "godaddy", "namecheap", "dreamhost", "hostgator",
    "limelight", "stackpath", "psychz", "quadranet", "hivelocity", "frantech",
    "buyvm", "netcup", "constant company", "wholesale", "censys", "shodan",
    "internet census", "driftnet", "binaryedge",
)
# ASN org names that mean "this is a consumer ISP / residential network".
RES_KEYWORDS = (
    "comcast", "xfinity", "at&t", "verizon", "spectrum", "charter", "cox",
    "centurylink", "lumen", "frontier", "t-mobile", "tmobile", "sprint",
    "cable", "broadband", "cablevision", "rcn", "wideopenwest", "mediacom",
    "suddenlink", "windstream", "fios", "dsl", "fiber", "wireless", "cellular",
    "telecom", "communications", "bell", "rogers", "telus", "shaw", "videotron",
    "virgin", "deutsche telekom", "vodafone", "orange", "telefonica", "ziggo",
    "kpn", "sbcglobal", "bellsouth",
)
# reverse-DNS names that identify a known crawler/scanner outright.
RDNS_BOT = (
    "googlebot", "bingbot", "baiduspider", "yandex", "ahrefs", "semrush",
    "mj12", "dotbot", "petalbot", "applebot", "gptbot", "ccbot", "amazonbot",
    "crawl", "spider", "bot.", "scan", "censys", "shodan", "masscan",
    "internet-census", "research", "driftnet", "stretchoid", "binaryedge",
)
# reverse-DNS names that look like datacenter/cloud hosts.
RDNS_DC = (
    "amazonaws.com", "ec2-", "googleusercontent", "1e100.net", "azure",
    "cloudapp", "linode", "ovh", "hetzner", "your-server.de", "vultr",
    "digitalocean", "leaseweb", "contabo", "scaleway", "m247", "dns.google",
    "one.one.one.one", "quadranet", "colocrossing",
)
# reverse-DNS names that look like residential/ISP hosts.
RDNS_RES = (
    "comcast.net", "hsd1", ".res.", "dyn", "dynamic", "dsl", "cable", "fios",
    "rr.com", "charter", "cox.net", "spectrum", "broadband", "client",
    "customer", "myvzw", "t-mobile", "sbcglobal", "bellsouth", "verizon.net",
    "lightspeed",
)


# --- parsing -----------------------------------------------------------------
def parse_ts(s):
    s = s.strip()
    for fmt in ("%Y-%m-%d %H:%M:%S %z", "%Y-%m-%d %H:%M:%S"):
        try:
            dt = datetime.strptime(s, fmt)
            return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)
        except ValueError:
            continue
    return None


@contextlib.contextmanager
def _open_log(path):
    """Yield a text handle for `path`, or stdin when path == '-'."""
    if path == "-":
        yield sys.stdin
        return
    fh = open(path, "r", errors="replace")
    try:
        yield fh
    finally:
        fh.close()


def read_log(path, exclude):
    """Return (hits_by_ip, stats). hits_by_ip[ip] = [(dt, selector), ...].
    Only 'serving' lines are kept; excluded IPs are dropped."""
    hits = {}
    total = served = excluded_lines = malformed = 0
    excluded_ips = set()
    with _open_log(path) as fh:
        for line in fh:
            line = line.rstrip("\n")
            if not line.strip():
                continue
            total += 1
            m = LINE_RE.match(line)
            if not m:
                malformed += 1
                continue
            if m.group("status").strip() != "serving":
                continue
            served += 1
            ip = m.group("ip").strip()
            if ip in exclude:
                excluded_lines += 1
                excluded_ips.add(ip)
                continue
            dt = parse_ts(m.group("ts"))
            if dt is None:
                malformed += 1
                continue
            hits.setdefault(ip, []).append((dt, m.group("sel")))
    for ip in hits:
        hits[ip].sort(key=lambda t: t[0])
    stats = {
        "total": total, "served": served, "excluded_lines": excluded_lines,
        "excluded_ips": sorted(excluded_ips), "malformed": malformed,
    }
    return hits, stats


# --- reverse DNS (best-effort, cached per run) -------------------------------
def resolve_rdns(ips, enabled, workers=16, timeout=2.0):
    if not enabled:
        return {ip: None for ip in ips}
    old = socket.getdefaulttimeout()
    socket.setdefaulttimeout(timeout)

    def one(ip):
        try:
            return ip, socket.gethostbyaddr(ip)[0]
        except (socket.herror, socket.gaierror, OSError):
            return ip, None

    try:
        with ThreadPoolExecutor(max_workers=workers) as ex:
            return dict(ex.map(one, ips))
    finally:
        socket.setdefaulttimeout(old)


# --- ASN (offline MaxMind GeoLite2-ASN lookup) -------------------------------
def default_asn_db():
    env = os.environ.get("GEOLITE2_ASN_DB")
    if env:
        return env
    cache = os.path.expanduser("~/.cache/gopher-cta/GeoLite2-ASN.mmdb")
    if os.path.exists(cache):
        return cache
    here = os.path.join(os.path.dirname(os.path.abspath(__file__)), "GeoLite2-ASN.mmdb")
    return here if os.path.exists(here) else cache


def download_asn_db(dest, license_key):
    url = ("https://download.maxmind.com/app/geoip_download"
           f"?edition_id=GeoLite2-ASN&license_key={license_key}&suffix=tar.gz")
    os.makedirs(os.path.dirname(dest) or ".", exist_ok=True)
    print(f"downloading GeoLite2-ASN -> {dest}", file=sys.stderr)
    with tempfile.NamedTemporaryFile(suffix=".tar.gz", delete=False) as tmp:
        with urllib.request.urlopen(url, timeout=60) as r:
            tmp.write(r.read())
        tarpath = tmp.name
    try:
        with tarfile.open(tarpath) as tf:
            member = next(m for m in tf.getmembers() if m.name.endswith("GeoLite2-ASN.mmdb"))
            src = tf.extractfile(member)
            with open(dest, "wb") as out:
                out.write(src.read())
    finally:
        os.unlink(tarpath)
    print("done.", file=sys.stderr)


def load_asn_reader(path):
    try:
        import maxminddb  # noqa
    except ImportError:
        return None, "maxminddb not installed (pip install maxminddb)"
    if not path or not os.path.exists(path):
        return None, f"no ASN DB at {path} (run with --download-asn, or --asn-db PATH)"
    try:
        return __import__("maxminddb").open_database(path), None
    except Exception as e:  # noqa: BLE001 — surface any open error as degraded mode
        return None, f"could not open ASN DB: {e}"


def asn_lookup(reader, ip):
    if reader is None:
        return None, None
    try:
        rec = reader.get(ip)
    except Exception:  # noqa: BLE001 — bad/IPv6 address etc. -> unknown
        return None, None
    if not rec:
        return None, None
    return rec.get("autonomous_system_number"), rec.get("autonomous_system_organization")


# --- classification ----------------------------------------------------------
def classify_network(org, rdns):
    """Return (kind, reasons). kind in residential/datacenter/mixed/unknown."""
    reasons = []
    o, r = (org or "").lower(), (rdns or "").lower()
    dc = res = False
    if r:
        if any(k in r for k in RDNS_DC):
            dc = True; reasons.append(f"rDNS '{rdns}' looks like a datacenter/cloud host")
        if any(k in r for k in RDNS_RES):
            res = True; reasons.append(f"rDNS '{rdns}' looks like a residential/ISP host")
    if o:
        if any(k in o for k in DC_KEYWORDS):
            dc = True; reasons.append(f"ASN org '{org}' matches a hosting/cloud provider")
        if any(k in o for k in RES_KEYWORDS):
            res = True; reasons.append(f"ASN org '{org}' matches a consumer ISP")
    if dc and not res:
        return "datacenter", reasons
    if res and not dc:
        return "residential", reasons
    if dc and res:
        return "mixed", reasons
    return "unknown", reasons


def fmt_dur(seconds):
    seconds = int(round(seconds))
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m{seconds % 60:02d}s"
    return f"{seconds // 3600}h{(seconds % 3600) // 60:02d}m"


def analyze(ip, hits, rdns, asn_org, asn_num):
    times = [h[0] for h in hits]
    n = len(hits)
    span = (times[-1] - times[0]).total_seconds() if n > 1 else 0.0
    gaps = [(times[i + 1] - times[i]).total_seconds() for i in range(n - 1)]
    median_gap = statistics.median(gaps) if gaps else None
    bursty = n >= 6 and median_gap is not None and median_gap < 1.5

    kind, net_reasons = classify_network(asn_org, rdns)

    bot = human = 0
    why = list(net_reasons)
    is_named_bot = bool(rdns) and any(k in rdns.lower() for k in RDNS_BOT)
    if is_named_bot:
        bot += 4; why.append("rDNS identifies a known crawler/scanner")
    if kind == "datacenter":
        bot += 2
    elif kind == "residential":
        human += 2
    if bursty:
        bot += 2
        why.append(f"bursty access ({n} hits ~{median_gap:.1f}s apart) — machine-paced")
    if median_gap is not None and not bursty and 1.0 <= median_gap <= 1800:
        human += 1
        why.append(f"human-paced gaps (~{fmt_dur(median_gap)} between hits)")
    if n == 1:
        why.append("single request — weak signal")

    if bot == 0 and human == 0:
        verdict = "UNKNOWN"
    elif bot >= human + 3:
        verdict = "BOT/CRAWLER"
    elif bot > human:
        verdict = "LIKELY BOT"
    elif human >= bot + 2:
        verdict = "HUMAN"
    elif human > bot:
        verdict = "LIKELY HUMAN"
    else:
        verdict = "UNKNOWN"

    return {
        "ip": ip, "n": n, "span": span, "median_gap": median_gap, "bursty": bursty,
        "kind": kind, "org": asn_org, "asn": asn_num, "rdns": rdns,
        "verdict": verdict, "why": why,
        "first": times[0], "last": times[-1],
        "trail": [h[1] for h in hits],
    }


# --- report ------------------------------------------------------------------
def render(records, stats, meta, max_trail):
    L = []
    p = L.append
    p("=" * 72)
    p("gopher-cta visitor report  —  who is hitting the gopher server")
    p("=" * 72)
    p(f"log        : {meta['log']}")
    p(f"asn db     : {meta['asn']}")
    p(f"reverse dns: {'on' if meta['rdns'] else 'off (--no-rdns)'}")
    if stats["served"]:
        win_a = min(r["first"] for r in records) if records else None
        win_b = max(r["last"] for r in records) if records else None
        if win_a and win_b:
            p(f"window     : {win_a:%Y-%m-%d %H:%M} -> {win_b:%Y-%m-%d %H:%M} "
              f"({fmt_dur((win_b - win_a).total_seconds())})")
    p(f"lines      : {stats['served']} served / {stats['total']} total"
      + (f" / {stats['malformed']} unparsed" if stats["malformed"] else ""))
    if stats["excluded_ips"]:
        p(f"excluded   : {stats['excluded_lines']} lines from "
          f"{', '.join(stats['excluded_ips'])}")
    p(f"visitors   : {len(records)} distinct IP(s), ranked by request count")
    p("")
    p("verdict heuristic: residential ASN + human-paced trail => human;")
    p("                   datacenter ASN / crawler rDNS + bursty trail => bot.")
    p("")

    if not records:
        p("(no visitor traffic after exclusions)")
        return "\n".join(L) + "\n"

    for i, r in enumerate(records, 1):
        org = r["org"] or "(unknown — no ASN match)"
        asn = f"  [AS{r['asn']}]" if r["asn"] else ""
        rdns = r["rdns"] or "(no reverse DNS)"
        timing = f"{r['n']} hits"
        if r["n"] > 1:
            timing += f" in {fmt_dur(r['span'])}"
            if r["median_gap"] is not None:
                timing += f" · median gap {fmt_dur(r['median_gap'])}"
            timing += " · bursty" if r["bursty"] else " · paced"
        trail = r["trail"]
        shown = " -> ".join(trail[:max_trail])
        if len(trail) > max_trail:
            shown += f"  (+{len(trail) - max_trail} more)"

        p("-" * 72)
        p(f"#{i}  {r['ip']} — {org} ({r['kind']}){asn}")
        p(f"    rDNS    : {rdns}")
        p(f"    activity: {timing}")
        p(f"    verdict : {r['verdict']}")
        p(f"    why     : {'; '.join(r['why']) if r['why'] else 'no distinguishing signal'}")
        p(f"    trail   : {shown}")
    p("-" * 72)
    return "\n".join(L) + "\n"


# --- temporal distribution ---------------------------------------------------
def parse_bucket(s):
    """'day' | 'hour' | '30m' | '15' -> (is_day, minutes)."""
    s = s.strip().lower()
    if s in ("day", "d", "1d"):
        return True, 1440
    if s in ("hour", "hr", "h", "1h", "60m"):
        return False, 60
    m = re.match(r"^(\d+)\s*m(in)?$", s)
    if m:
        return False, int(m.group(1))
    if s.isdigit():
        return False, int(s)
    raise ValueError(f"bad --bucket {s!r} (use day, hour, or N / Nm)")


def parse_release(s):
    s = s.strip()
    for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S",
                "%Y-%m-%dT%H:%M", "%Y-%m-%d"):
        try:
            dt = datetime.strptime(s, fmt)
            return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)
        except ValueError:
            continue
    raise ValueError(f"unparseable --release timestamp {s!r}")


def _vclass(verdict):
    if verdict in ("HUMAN", "LIKELY HUMAN"):
        return "h"
    if verdict in ("BOT/CRAWLER", "LIKELY BOT"):
        return "b"
    return "q"


def render_timeline(flat, ip_verdict, bucket_spec, release):
    """flat = [(dt, ip), ...] (already excludes dropped IPs)."""
    is_day, minutes = bucket_spec
    step = timedelta(days=1) if is_day else timedelta(minutes=minutes)
    label_fmt = "%Y-%m-%d" if is_day else "%m-%d %H:%M"
    unit = "day" if is_day else (f"{minutes}min" if minutes != 60 else "hour")

    def floor(dt):
        if is_day:
            return dt.replace(hour=0, minute=0, second=0, microsecond=0)
        tm = (dt.hour * 60 + dt.minute) // minutes * minutes
        return dt.replace(hour=tm // 60, minute=tm % 60, second=0, microsecond=0)

    L = []
    p = L.append
    p("=" * 72)
    p(f"temporal distribution — per {unit} (UTC), served hits, humans vs bots")
    p("=" * 72)
    if not flat:
        p("(no traffic to bucket)")
        return "\n".join(L) + "\n"

    buckets = {}  # start -> {n, h, b, q, ips:set}
    for dt, ip in flat:
        b = floor(dt)
        slot = buckets.setdefault(b, {"n": 0, "h": 0, "b": 0, "q": 0, "ips": set()})
        slot["n"] += 1
        slot[_vclass(ip_verdict.get(ip, ""))] += 1
        slot["ips"].add(ip)

    start, end = floor(min(b for b, _ in flat)), floor(max(b for b, _ in flat))
    n_slots = int((end - start) / step) + 1
    if n_slots > 400:
        p(f"(warning: {n_slots} buckets at this resolution — try a bigger --bucket)")

    peak = max(s["n"] for s in buckets.values())
    width = 40
    released = False
    cur = start
    while cur <= end:
        if release is not None and not released and cur + step > release:
            p(f"  {'·' * 16}  ── release {release:%Y-%m-%d %H:%M} UTC ──")
            released = True
        s = buckets.get(cur)
        n = s["n"] if s else 0
        bar = "█" * round(n / peak * width) if peak and n else ""
        split = f"{s['h']}/{s['b']}/{s['q']}" if s else "0/0/0"
        mark = "  <- peak" if s and n == peak else ""
        p(f"  {cur.strftime(label_fmt)}  {n:>4}  h/b/?={split:<9} │{bar}{mark}")
        cur += step

    p("")
    p("  legend: h=human(+likely)  b=bot(+likely)  ?=unknown   bar scaled to peak")

    if release is not None:
        def summarize(pred):
            sub = [(dt, ip) for dt, ip in flat if pred(dt)]
            ips = {ip for _, ip in sub}
            humans = {ip for _, ip in sub if _vclass(ip_verdict.get(ip, "")) == "h"}
            bots = {ip for _, ip in sub if _vclass(ip_verdict.get(ip, "")) == "b"}
            return len(sub), len(ips), len(humans), len(bots)

        bn, bip, bh, bb = summarize(lambda d: d < release)
        an, aip, ah, ab = summarize(lambda d: d >= release)
        p("")
        p(f"  before/after release {release:%Y-%m-%d %H:%M} UTC:")
        p(f"    before: {bn:>4} hits · {bip:>2} IPs ({bh} human, {bb} bot)")
        p(f"    after : {an:>4} hits · {aip:>2} IPs ({ah} human, {ab} bot)")
    return "\n".join(L) + "\n"


# --- main --------------------------------------------------------------------
def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Read-only geomyidae access-log visitor analysis.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--log", default=LOG_DEFAULT,
                    help=f"access log path, or '-' for stdin (default {LOG_DEFAULT})")
    ap.add_argument("--source-label", default=None, metavar="TEXT",
                    help="label shown for the log source in the report header (e.g. host:path)")
    ap.add_argument("--exclude-ip", action="append", default=None, metavar="IP",
                    help=f"drop this IP (repeatable; default once: {SELF_IP_DEFAULT})")
    ap.add_argument("--asn-db", default=None, metavar="PATH",
                    help="GeoLite2-ASN.mmdb path (default: $GEOLITE2_ASN_DB / cache / scripts dir)")
    ap.add_argument("--download-asn", action="store_true",
                    help="download GeoLite2-ASN once (needs $MAXMIND_LICENSE_KEY) then continue")
    ap.add_argument("--license-key", default=os.environ.get("MAXMIND_LICENSE_KEY"),
                    help="MaxMind license key (or set $MAXMIND_LICENSE_KEY)")
    ap.add_argument("--no-rdns", action="store_true", help="skip reverse-DNS lookups")
    ap.add_argument("--max-trail", type=int, default=10, metavar="N",
                    help="selectors to show per visitor before collapsing (default 10)")
    ap.add_argument("--timeline", action="store_true",
                    help="append a temporal distribution histogram (hits over time)")
    ap.add_argument("--bucket", default="hour", metavar="SIZE",
                    help="timeline bin size: day, hour, or N / Nm minutes (default hour)")
    ap.add_argument("--release", default=None, metavar="TS",
                    help="mark a release time (UTC, e.g. '2026-06-25 12:00') and split before/after; implies --timeline")
    ap.add_argument("--out", default=None, metavar="FILE", help="also write the report to FILE")
    args = ap.parse_args(argv)

    exclude = set(args.exclude_ip if args.exclude_ip is not None else [SELF_IP_DEFAULT])
    exclude.discard("")  # allow `--exclude-ip ''` to mean "exclude nothing"

    asn_path = args.asn_db or default_asn_db()
    if args.download_asn:
        if not args.license_key:
            ap.error("--download-asn needs --license-key or $MAXMIND_LICENSE_KEY")
        download_asn_db(asn_path, args.license_key)

    if args.log != "-" and not os.path.exists(args.log):
        ap.error(f"log not found: {args.log}")

    hits, stats = read_log(args.log, exclude)

    reader, asn_note = load_asn_reader(asn_path)
    asn_desc = asn_path if reader else f"{asn_path} — {asn_note}"

    ips = list(hits.keys())
    rdns_map = resolve_rdns(ips, enabled=not args.no_rdns)

    records = []
    for ip in ips:
        num, org = asn_lookup(reader, ip)
        records.append(analyze(ip, hits[ip], rdns_map.get(ip), org, num))
    records.sort(key=lambda r: (r["n"], r["span"]), reverse=True)

    log_label = args.source_label or ("(stdin)" if args.log == "-" else args.log)
    meta = {"log": log_label, "asn": asn_desc, "rdns": not args.no_rdns}
    report = render(records, stats, meta, args.max_trail)

    if args.timeline or args.release:
        release = parse_release(args.release) if args.release else None
        bucket_spec = parse_bucket(args.bucket)
        ip_verdict = {r["ip"]: r["verdict"] for r in records}
        flat = [(dt, ip) for ip in hits for dt, _ in hits[ip]]
        report += "\n" + render_timeline(flat, ip_verdict, bucket_spec, release)

    sys.stdout.write(report)
    if args.out:
        with open(args.out, "w") as fh:
            fh.write(report)
        print(f"\n[report written to {args.out}]", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
