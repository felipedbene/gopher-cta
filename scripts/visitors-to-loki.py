#!/usr/bin/env python3
"""visitors-to-loki.py — push enriched visitor NDJSON to Grafana Loki.

Reads the NDJSON emitted by `gopher-visitors.py --format ndjson` on stdin (one
JSON object per hit, each with a `ts_ns` nanosecond timestamp) and POSTs it to a
Loki instance via the push API (`/loki/api/v1/push`).

Design (per Loki guidance):
  - Stream labels are STATIC and low-cardinality: {job, host} (+ any --extra-label).
  - Every high-cardinality field (ip, selector, org, asn, rdns, verdict, vclass)
    stays in the LOG LINE BODY — query them in Grafana with LogQL `| json`, e.g.
        {job="gopher-cta-visitors"} | json | vclass="h"
        sum by (verdict) (count_over_time({job="gopher-cta-visitors"} | json [1h]))
  - Putting IP/selector in labels would explode cardinality — don't.

stdlib only (urllib). Config via flags or env:
    LOKI_URL   base url (e.g. http://host:3100 or https://logs-prod-xx.grafana.net)
    LOKI_USER / LOKI_PASS   basic auth (Grafana Cloud: user=numeric id, pass=token)
    LOKI_TENANT             X-Scope-OrgID for multi-tenant Loki
    LOKI_HOST_LABEL         value of the `host` label (default gopher.debene.dev)

Examples:
    ... --format ndjson | scripts/visitors-to-loki.py --dry-run         # inspect payload
    ... --format ndjson | LOKI_URL=http://10.0.0.5:3100 scripts/visitors-to-loki.py
"""
import argparse
import base64
import json
import os
import sys
import urllib.error
import urllib.request


def main(argv=None):
    ap = argparse.ArgumentParser(description="Push visitor NDJSON to Loki.")
    ap.add_argument("--loki-url", default=os.environ.get("LOKI_URL"),
                    help="Loki base url or full .../push (env LOKI_URL)")
    ap.add_argument("--job", default="gopher-cta-visitors", help="`job` label")
    ap.add_argument("--host", default=os.environ.get("LOKI_HOST_LABEL", "gopher.debene.dev"),
                    help="`host` label (env LOKI_HOST_LABEL)")
    ap.add_argument("--extra-label", action="append", default=[], metavar="K=V",
                    help="add a static stream label (repeatable); keep low-cardinality")
    ap.add_argument("--tenant", default=os.environ.get("LOKI_TENANT"),
                    help="X-Scope-OrgID for multi-tenant Loki (env LOKI_TENANT)")
    ap.add_argument("--user", default=os.environ.get("LOKI_USER"),
                    help="basic-auth user (env LOKI_USER)")
    ap.add_argument("--password", default=os.environ.get("LOKI_PASS"),
                    help="basic-auth password/token (env LOKI_PASS)")
    ap.add_argument("--batch", type=int, default=1000, help="entries per push (default 1000)")
    ap.add_argument("--dry-run", action="store_true",
                    help="print a sample payload and counts; send nothing")
    args = ap.parse_args(argv)

    if not args.loki_url and not args.dry_run:
        ap.error("need --loki-url or $LOKI_URL (or pass --dry-run)")

    values, bad = [], 0
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            ts = json.loads(line)["ts_ns"]
        except (ValueError, KeyError):
            bad += 1
            continue
        values.append([str(ts), line])
    values.sort(key=lambda v: int(v[0]))  # Loki wants entries time-ordered

    labels = {"job": args.job, "host": args.host}
    for kv in args.extra_label:
        k, _, v = kv.partition("=")
        if k:
            labels[k] = v

    url = (args.loki_url or "http://localhost:3100").rstrip("/")
    if not url.endswith("/push"):
        url += "/loki/api/v1/push"

    headers = {"Content-Type": "application/json"}
    if args.tenant:
        headers["X-Scope-OrgID"] = args.tenant
    if args.user:
        tok = base64.b64encode(f"{args.user}:{args.password or ''}".encode()).decode()
        headers["Authorization"] = "Basic " + tok

    total = len(values)
    print(f"{total} entries ({bad} skipped) · labels={labels} · -> {url}", file=sys.stderr)
    if total == 0:
        print("nothing to push", file=sys.stderr)
        return 0

    sent = 0
    for i in range(0, total, args.batch):
        chunk = values[i:i + args.batch]
        payload = {"streams": [{"stream": labels, "values": chunk}]}
        if args.dry_run:
            if i == 0:
                sample = {"streams": [{"stream": labels, "values": chunk[:3]}]}
                print(json.dumps(sample, indent=2, ensure_ascii=False))
            sent += len(chunk)
            continue
        req = urllib.request.Request(
            url, data=json.dumps(payload).encode(), headers=headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                resp.read()
                sent += len(chunk)
        except urllib.error.HTTPError as e:
            body = e.read().decode("utf-8", "replace")[:400]
            print(f"push failed [{e.code} {e.reason}]: {body}", file=sys.stderr)
            return 1
        except urllib.error.URLError as e:
            print(f"connection failed: {e.reason}", file=sys.stderr)
            return 1

    verb = "DRY-RUN, would send" if args.dry_run else "pushed"
    print(f"{verb} {sent}/{total} entries to Loki", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
