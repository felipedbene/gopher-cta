# Deploying gopher-cta

Operational runbook for the live deployment at **`gopher://gopher.debene.dev:70/`**.

> This documents the *running* deployment, not the dev/quickstart flow in the
> README. For local browsing on a laptop, see the README quickstart instead.

---

## What's running

Two containers on the production VPS (RackNerd Chicago, `192.210.238.140`),
orchestrated by Docker Compose:

| Container | Image | Role |
| --- | --- | --- |
| `gopher-cta-fetcher` | `gopher-cta-local:amd64` (or `ghcr.io/felipedbene/gopher-cta:latest`) | Polls the CTA feed every 30s and renders a static gopher tree into `./public` |
| `geo` | `geomyidae:local` (built from `deploy/Dockerfile.geomyidae`) | The gopher daemon; serves `./public/current` on the wire |

They share the `./public` directory: the fetcher writes it, geomyidae serves it
read-only.

### How publishing works (and why it matters)

The fetcher does **not** edit a live tree in place. Each cycle it:

1. renders a complete tree into a fresh `public/out-<nanos>/` snapshot,
2. atomically flips the `public/current` symlink to the new snapshot (`rename(2)`),
3. garbage-collects old snapshots, keeping the newest `KEEP_SNAPSHOTS` (3) plus
   whatever `current` points at.

geomyidae is pointed at `current/`, so a reader always sees a whole tree, never a
half-written one.

**Consequence:** anything that must be served has to be written *into every
snapshot* by the fetcher. A file dropped into `public/` by hand will not be served
— it's not inside `current/`, and the next flip would ignore it anyway. This is
why `robots.txt` and `caps.txt` are embedded in the fetcher binary
(`include_bytes!`) and written into each snapshot by `write_tree()`, not shipped as
loose files.

---

## Prerequisites

- Docker + Docker Compose on the VPS.
- A checkout of this repo on the VPS (the deploy host builds the local image).
- A `.env` file next to `docker-compose.yml` (gitignored):

  ```dotenv
  CTA_TRAIN_API_KEY=<cta train tracker key>   # unset => offline fixture mode
  GOPHER_HOST=gopher.debene.dev               # advertised host in menu links
  GOPHER_PORT=70                              # host port (mapped to 7070 in-container)
  # optional AI narration:
  # CTA_AI_BASE=...
  # CTA_HOME_...=...
  ```

- Inbound TCP **70** open on the VPS firewall.

---

## Image strategy

There are two ways to source the fetcher image; the deployment picks one via
`docker-compose.override.yml`.

- **CI image (default, base compose):** `ghcr.io/felipedbene/gopher-cta:latest`,
  built multi-arch by GitHub Actions on every push to `master`, with
  `pull_policy: always`.
- **Local build (current production, via override):** `gopher-cta-local:amd64`,
  built on the VPS from the working tree, with `pull_policy: never`.

`docker-compose.override.yml` pins the local image and is auto-merged by Compose.
The geomyidae service's `command` in the override (`-h gopher.debene.dev -o 70`)
is **appended** to the image ENTRYPOINT (`geomyidae -d -b /srv/current -p 7070`),
not substituted — so the base dir and listen port come from the ENTRYPOINT, and
the override only adds the advertised host/port. `-p` is the real listen port
(7070); `-o` is the *advertised* port (70, the host-mapped one clients reach).

---

## Deploy / update procedure

This is the validated path for shipping a code change (the common case):

```bash
cd ~/gopher-cta
git pull                       # get the change you're deploying
docker compose build fetcher   # rebuild gopher-cta-local:amd64 from the working tree
docker compose up -d           # recreate fetcher (new image) + apply override to geo
sleep 35                       # wait for at least one publish cycle (~30s)
```

`docker compose up -d` recreates only the containers whose config or image
changed, so geomyidae isn't rebuilt (it has no source change). If you're on the CI
image instead of the local override, swap the build step for
`docker compose pull fetcher`.

After it settles, **verify** (see below) before announcing anything.

---

## Verification

From any host that can reach port 70:

```bash
gph() { printf '%s\r\n' "$1" | nc -w5 gopher.debene.dev 70; }

gph "/robots.txt"          # expect: the policy file (User-agent: * / Disallow: /train/)
gph "/train/906.txt"       # expect: a live train detail page (pick a run that's running)
gph "/map.txt"             # expect: the braille map with a recent feed timestamp
```

`nc`-free alternative:

```bash
gph() { python3 -c "import socket,sys;s=socket.create_connection(('gopher.debene.dev',70),5);s.sendall((sys.argv[1]+chr(13)+chr(10)).encode());print(s.recv(8000).decode('utf-8','replace'))" "$1"; }
```

What "good" looks like:

- `/robots.txt` returns the policy text with **no leading `3`** and no `Err` row.
  (A `3...Err` row is geomyidae's error item type — the selector wasn't found.)
- A live run returns data with a fresh `predicted` timestamp.
- A not-in-service run (e.g. `/train/999999.txt`) returns a `3...Err` row — this is
  **expected and fine**: train pages are ephemeral and fenced from crawlers by
  `robots.txt`.

---

## Why `/train/` is fenced (crawler policy)

CTA run numbers churn as trains enter and leave service, so per-train selectors
(`/train/<run>.txt`) appear and vanish within minutes. Indexing them just fills a
search index (e.g. Floodgap's Veronica-2) with dead links. Everything else — maps,
atlas, landmarks, per-line menus, narrative pages, about, caps — has a **stable
selector** and is safe to index even though its content is live. `robots.txt`
therefore disallows `/train/` only, and sets `Crawl-delay: 30` to match the
publish cadence.

---

## Operational notes

- **Ports:** host `70` → container `7070` (`GOPHER_PORT:7070` mapping + geomyidae
  `-p 7070`). geomyidae runs unprivileged (`USER nobody`), hence the high internal
  port.
- **Restart:** both services are `restart: unless-stopped`, so they come back
  after a reboot. The fetcher resumes publishing; geomyidae resumes serving
  `current/`.
- **Offline mode:** with `CTA_TRAIN_API_KEY` unset, the fetcher serves the bundled
  fixture (`fixtures/positions.json`) — useful for validating a deploy without
  hitting the live feed.
- **Logs:** `docker compose logs -f fetcher` shows each publish
  (`[fetch] published out-<ts> (<n> trains) -> public/current`).
- **Disk:** snapshots are GC'd to the newest 3 + current, so `public/` stays
  bounded.

---

## Troubleshooting

### `robots.txt` (or any embedded file) 404s with `3...Err`

Almost always a **stale local image**. `robots.txt`/`caps.txt` are compiled into
the fetcher binary, so a running binary that doesn't emit them was built before
that code existed. The fetcher republishing fresh train data is *not* evidence the
binary is current — only a rebuild updates the embedded files.

Fix: `git pull && docker compose build fetcher && docker compose up -d`, wait one
cycle, re-verify. Confirm the commit is actually in this checkout with:

```bash
git -C ~/gopher-cta log --oneline -- robots.txt src/fetch.rs
```

### Menu links / error rows show `localhost` instead of `gopher.debene.dev`

geomyidae's advertised host (`-h`) isn't applied. The override sets it, but `geo`
may not have been recreated. Force it:

```bash
docker compose up -d geomyidae
docker inspect geo --format '{{.Args}}'   # confirm -h gopher.debene.dev -o 70 present
```

Cosmetic — it affects the `server`/`port` tokens geomyidae substitutes into menus
and error rows, not whether content serves.

### Can't reach port 70 at all, but 80/443 work

Server's up; port 70 is filtered between you and the VPS (some egress proxies block
the gopher port). Test from a different network, or check the VPS firewall allows
inbound 70.

### geomyidae won't start after a Dockerfile rebuild

The geomyidae image clones from `git://bitreich.org` (port 9418). The build host
needs 9418 egress. If the clone fails, the image build fails — rebuild from a host
that allows it, or pin `GEOMYIDAE_REF` to a cached layer.
