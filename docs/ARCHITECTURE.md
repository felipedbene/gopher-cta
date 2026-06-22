# gopher-cta — Architecture

How the pieces fit today, and the plan for moving the live deployment onto
Kubernetes. For day-to-day working notes (commands, gotchas, conventions) see
[`../CLAUDE.md`](../CLAUDE.md); for user-facing usage see [`../README.md`](../README.md).

---

## Overview

gopher-cta turns live CTA 'L' train data into a **static gopher tree** and lets a
hardened gopher daemon serve it. The core program is a **fetcher**, not a server:
it renders files and atomically publishes them; the protocol is somebody else's
job (geomyidae). A second, independent data path pulls AI narrative panels
(Dispatch / SITREP / Event Advisory) from a remote Worker.

Two design choices drive everything:

- **Render-to-files, don't serve.** No bespoke socket/selector code; atomic
  snapshot publishing means readers never see a half-written tree; serving scales
  by copying files.
- **The train map is the fast path and depends on nothing slow.** Live positions
  refresh on a tight cadence; the AI narrative is best-effort and can never block
  or break the map.

---

## Current architecture

### Data flow

```
   CTA Train Tracker API ─────────────┐
   (HTTPS, key or fixture)            │  positions (≈30s)
                                      ▼
   Cloudflare Worker ──────────┐   ┌──────────────────────────────┐
   (AI: dispatch/sitrep/events)│   │          fetcher             │
   gopher reads, never gens    │   │  ┌────────────────────────┐  │
                               └──▶│  │ narration poller (task)│  │
            narrative (1–30m)      │  │  Arc<Mutex<View>>      │  │
                                   │  └───────────┬────────────┘  │
                                   │     render core (pure)       │
                                   │   braille map · char atlas · │
                                   │   menus · train · AI pages   │
                                   └───────────────┬──────────────┘
                                                   │ atomic publish
                                     out/out-<ts>/ … → flip out/current
                                                   │
                                          geomyidae serves out/current
                                                   │  gopher (RFC 1436)
                                                   ▼
                                            gopher clients
```

### Components

- **Fetcher** (`src/`, one Rust binary). Loops every `--interval` (default 30s):
  fetch CTA positions → render the whole tree into a fresh `out-<ts>/` → flip the
  `current` symlink (atomic rename) → GC old snapshots. A **detached narration
  poller** task hits the Worker on slow per-panel cadences and updates a shared
  `Arc<Mutex<NarrationView>>`; each publish reads a clone — never awaits the
  network on the publish path.
- **geomyidae** (external daemon). Serves `out/current` read-only. The only
  daemon-specific code we own is the `.gph` menu serializer in `fetch.rs`
  (`render_menu_index`); switching daemons rewrites just that.
- **Sources.** CTA Train Tracker (live, key via `CTA_TRAIN_API_KEY`; falls back
  to the bundled `fixtures/positions.json`) and the AI Worker
  (`CTA_AI_BASE`, default the production worker). The Worker does all DeepSeek
  generation + caching server-side; gopher-cta is purely a reader.

### Module map (`src/`)

| Module | Responsibility |
|--------|----------------|
| `braille.rs` | 2×4-dot monochrome canvas (`set(px,py)` ORs a dot). |
| `project.rs` | **The** km-based projection `lat/lon → braille pixel`. Single source of truth; every layer reuses it. |
| `render.rs` | Pure render core: braille map, menus, train/about pages, daemon-agnostic `Entry` model. |
| `atlas.rs` | Char-cell geographic atlas. Rasterizes shoreline+landmarks once, clones per publish, paints trains. Reuses `project` collapsed to a cell. |
| `narration.rs` | AI panels: background Worker poller + pure page renderers. |
| `fetch.rs` | Process loop, atomic publish (`current` symlink + GC), geomyidae `.gph`. |
| `transit.rs` | `TransitSource` trait, `CtaSource` (live+fixture), Metra stub. |

### Rendering surfaces (the published tree)

| Path | Type | Content |
|------|------|---------|
| `index.gph` | menu | root: links to every surface + per-line counts |
| `map.txt` | text | braille geographic plot of live trains (high-res, monochrome) |
| `atlas.txt` | text | char-cell map: shoreline `#` + "LAKE MICHIGAN" label + landmarks `A`–`N` (by id) + trains as heading arrows `^ > v <`, ASCII, lettered legend |
| `map.ansi` / `atlas.ansi` | text | colour (ANSI 256) variants of the two maps — trains by CTA line colour; for `curl`/`cat`, plain pages kept for strict clients |
| `dispatch.txt` | text | AI one-liner + authoritative feed stats |
| `sitrep.txt` | text | AI alerts summary for the home station |
| `events.txt` | text | AI event advisory |
| `<line>/index.gph` | menu | per-line running trains |
| `train/<run>.txt` | text | per-train detail |
| `about.txt` | text | canvas/projection params |

Two map surfaces on purpose: braille gives sub-cell **resolution** but no
identity (every dot is a dot); the char atlas gives **legibility** (distinct
glyphs per feature, z-ordered) at lower resolution. Both derive from the same
projection, so they're pixel-locked to each other.

### Invariants

- One projection (`project::project`); never write a second.
- Geo rasterized once into a base grid, then cloned per publish.
- Atlas z-order (painter's): shoreline (1) < landmarks (3) < trains (5).
- No lake fill — coastline edge only. Landmark labels in a numbered legend, never
  inline.
- The train fast path never blocks on or hard-depends on narration.
- gopher correctness: type-0 for text bodies, type-1 for menus.

### Current deployment — local docker (felipe's Mac Studio, `10.0.10.69`)

Not Kubernetes today. Two containers, a shared bind mount:

```
  ┌─────────────────────── Mac Studio (10.0.10.69) ───────────────────────┐
  │                                                                        │
  │   gopher-cta-fetcher                         geo (geomyidae:local)     │
  │   image ghcr.io/felipedbene/gopher-cta       -p 7070:7070              │
  │   --env-file .env --interval 30   ┌────────┐ serves /srv/current       │
  │   writes ───────────────────────▶ │ public │ ◀─────────── reads (ro)   │
  │                                   └────────┘                           │
  │                              host bind mount                           │
  └────────────────────────────────────────────────────────────────────────┘
                                     │ :7070
                                     ▼
                          gopher://10.0.10.69:7070
```

geomyidae runs continuously and serves whatever the fetcher writes — a code
change only requires restarting the **fetcher** (geomyidae keeps running). If
`public/current` is missing, no fetcher is running (the classic "0 bytes from
gopher"). Exact redeploy commands live in `CLAUDE.md`.

---

## Future architecture

### Kubernetes plan

The intended production shape (`deploy/gopher-cta.yaml`, written but **not yet
applied**): the fetcher and geomyidae as **two containers in one pod** sharing an
`emptyDir`, fronted by a `LoadBalancer` service. No PVC — the tree is regenerated
every interval, so ephemeral storage is the right fit.

```
                 ┌──────────────── Pod: gopher-cta ────────────────┐
   CTA API ─────▶│  fetcher  ──writes──▶ emptyDir /srv ◀──reads──  │
   Worker  ─────▶│  (--interval 30)                    geomyidae   │
                 │                                      (-p 7070)   │
                 └───────────────────────┬─────────────────────────┘
                                         │ Service :70 → :7070 (LoadBalancer)
                                         ▼
                              MetalLB IP (10.0.100.x)  ──▶ gopher clients
   securityContext: nobody, drop ALL caps, readOnlyRootFS, fsGroup for the mount
```

Concrete steps to cut over from local docker to k8s:

1. **Fetcher image** — `ghcr.io/felipedbene/gopher-cta:latest`. *Done* (built,
   pushed). Ideally rebuild as a **multi-arch** manifest (amd64 cluster nodes +
   arm64) so it runs native everywhere.
2. **geomyidae image** — build from `deploy/Dockerfile.geomyidae` and push
   (`ghcr.io/felipedbene/geomyidae:latest`). **Not done yet** — this is the main
   missing artifact.
3. **Secrets/env** — set `CTA_TRAIN_API_KEY` in the manifest `Secret`; override
   `CTA_AI_BASE` / `CTA_HOME_MAPID` / `CTA_HOME_NAME` only if non-default.
4. **Pull secret** — if the ghcr packages are private, add a `ghcr-pull`
   `dockerconfigjson` secret to the namespace (pattern already used by `bbs` /
   `timescaledb`).
5. **Host token** — set geomyidae's `-h <addr>` to the address clients actually
   reach, so the `.gph` `server` placeholder resolves correctly. With MetalLB the
   service lands on a `10.0.100.x` IP (pool `10.0.100.100-200`); use that or a DNS
   name pointing at it.
6. **Apply** — `kubectl apply -f deploy/gopher-cta.yaml` (context
   `kubernetes-admin@kubernetes`), then validate `0/dispatch.txt` against the
   assigned IP. Retire the local fetcher/geomyidae containers once green.

Open questions for the k8s move: public vs private ghcr packages (drives step 4);
whether to expose on the LAN (LoadBalancer, like the other home services) or keep
it port-forward/ClusterIP; and pinning vs multi-arch scheduling across the mixed
amd64/arm64 nodes.

### Roadmap (beyond the k8s move)

- **Commit 3 — gopher-native nav.** *Done:* train heading arrows, the labelled
  lakefront, and the ANSI-colour `map.ansi` / `atlas.ansi` variants. *Remaining:*
  `/landmarks` as a type-1 menu with per-landmark detail pages (name, category,
  nearest stop if derivable). `Landmark.category` is already parsed for this.
- **Multi-arch images.** buildx `--platform linux/amd64,linux/arm64` so local
  (arm) and cluster (amd64) both run native; today's image is amd64-under-emulation
  locally.
- **Restart/persistence for the local deploy.** `--restart unless-stopped` or a
  small `docker-compose.yml` pairing fetcher + geomyidae, until/unless k8s lands.
- **More overlay layers** (same `chicago_geo.json` schema): Chicago River polyline
  (deferred), then potentially track centrelines / stations to fill the reserved
  z-order slots (2 = track, 4 = stations).
- **More agencies.** `MetraSource` is a real but empty `TransitSource` stub;
  implementing it against the Metra GTFS-realtime vehicle-positions feed plots
  regional rail with no render changes (widen the bbox). Same seam fits South Shore.
- **Service quality signals.** Bunching / headway detection from successive
  position snapshots (out of scope so far, but a natural fit for the dispatch page).
