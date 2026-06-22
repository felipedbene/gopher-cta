# CLAUDE.md — gopher-cta

Working notes so a session doesn't re-derive what's already known. If something
here is wrong, fix it here as you go. Fuller architecture (current + future, k8s
plan, roadmap) lives in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## What this is

A **fetcher**, not a server. It turns live CTA 'L' train positions into a
**static gopher tree** and an external daemon (**geomyidae**) serves it. The
custom protocol server was removed (`b0b4ea5`) — do not reintroduce one.

Flow: `CtaSource` → `render` → write `out/out-<ts>/` → atomic flip `out/current`
symlink → geomyidae serves `out/current`.

## Selectors are FILES (not live routes)

geomyidae prepends the gopher type, so browse `gopher://host:7070/0/map.txt`
(type 0 = text), `…/1/` for menus. On-disk tree per snapshot:

| File | What |
|------|------|
| `index.gph` | root menu (type-1) |
| `map.txt` | braille train map |
| `atlas.txt` | char-cell geo atlas: shoreline + landmarks + trains |
| `dispatch.txt` / `sitrep.txt` / `events.txt` | AI panels (see Narration) |
| `about.txt` | about |
| `<line>/index.gph` | per-line menu (`red blue brn g org p pink y`) |
| `train/<run>.txt` | per-train detail |

Note: the *old* custom server used `/map`; the static tree uses `/map.txt`.

## Module map (src/)

- `braille.rs` — 2×4-dot canvas; `set(px,py)` ORs a dot. Monochrome, no glyphs.
- `project.rs` — **the one** km-based projection `lat/lon → braille pixel`.
  TUNABLE bbox/`W` at top. **Reuse this everywhere; never write a 2nd projection.**
- `render.rs` — pure render core (map, menus, train/about pages) + `Entry` model.
- `atlas.rs` — char-cell geo atlas. Reuses `project::project` collapsed to a
  cell (`px/2,py/4`). Rasterizes shoreline+landmarks **once** (`Atlas::build`),
  clones per publish, paints trains. Data: `chicago_geo.json` via `include_str!`.
- `narration.rs` — AI panels from the Worker (see below).
- `fetch.rs` — loop, atomic publish (`current` symlink + GC), geomyidae `.gph`.
- `transit.rs` — `TransitSource` trait, `CtaSource` (live+fixture), Metra stub.
  CTA wire-parsing (`OneOrMany`) mirrors `~/Projects/cta-tui/src/cta.rs`.

### Invariants to preserve
Single projection · geo rasterized once then cloned · atlas z-order
shoreline(1) < landmarks(3) < trains(5) · no lake fill (coast edge only) ·
landmark labels in a numbered legend, never inline · type-0 text / type-1 menus.

## Narration (dispatch / sitrep / events)

Source is the deployed **Cloudflare Worker** — the SAME one `cta-tui` polls. We
are just another reader; **nothing here calls DeepSeek** (the Worker does
generation + caching server-side). `~/Projects/cta-tui/src/ai.rs` is the
reference contract.

- Base URL: `CTA_AI_BASE` (default `https://cta-track-grid.felipe-debene.workers.dev`).
- Endpoints: `/api/feed/narration` (dispatch) · `/api/alerts/summary?station=<mapid>&stn=<name>`
  (sitrep, per-station) · `/api/events/advisory`. Response `{summary, error}`.
- SITREP station: `CTA_HOME_MAPID` / `CTA_HOME_NAME` (default `41070` / Kedzie).
- `narration.rs` runs a **detached background poller** (Arc<Mutex<NarrationView>>)
  on slow cadences (dispatch ~1m, sitrep ~5m, events ~30m). Each publish reads a
  clone — **the train fast path never blocks on or hard-depends on narration.**
  Fetch error → keep last-good; never fetched → placeholder.

## Build / test / lint

```sh
cargo test                              # unit + integration; offline (fixture)
cargo clippy --all-targets -- -D warnings   # CI gate; toolchain Rust 1.95 (is_none_or OK)
cargo fmt --check
cargo run -- --once --out ./public      # render once; then: geomyidae -b ./public/current -p 7070
```

Offline by default: no `CTA_TRAIN_API_KEY` ⇒ bundled `fixtures/positions.json`
(18 trains). `chicago_geo.json` (14 landmarks, 17 shoreline pts) and the fixture
are embedded via `include_str!` — keep them at their paths; `Dockerfile` does
`COPY . .` so both land in the image.

Env: `CTA_TRAIN_API_KEY` `CTA_ROUTES` `GOPHER_OUT`/`--out` `--once` `--interval`
`CTA_AI_BASE` `CTA_HOME_MAPID` `CTA_HOME_NAME`. A gitignored `.env` is loaded at
startup (real env vars win).

## Deploy — read this before trying

Registry: **`ghcr.io/felipedbene/gopher-cta`** (per `deploy/gopher-cta.yaml`).
Cross-build for the cluster (mostly amd64 nodes):

```sh
docker buildx build --platform linux/amd64 -t ghcr.io/felipedbene/gopher-cta:latest --load .
```

**Pushing to ghcr from Claude Code does NOT work — the human must push.** The
Bash tool runs in a non-interactive macOS security session that cannot read the
`osxkeychain` credStore even after `security unlock-keychain` (error: "session
does not allow user interaction"), and `gh auth token` lacks `write:packages`.
So: **Claude builds + tags the image; felipe runs `docker push …` in his own
Terminal** (his keychain has the working ghcr creds).

**The live deploy is NOT k8s — it's two local docker containers on felipe's Mac
Studio** (this machine; its LAN IP is `10.0.10.69`). `deploy/gopher-cta.yaml`
(k8s 2-container pod + LoadBalancer) exists but is unapplied; the geomyidae image
isn't in ghcr. Don't go looking in the cluster.

Actual setup (`gopher://10.0.10.69:7070`):
1. `geo` — `geomyidae:local`, `-p 7070:7070`, mounts repo `public/ -> /srv`,
   serves `/srv/current`. Long-running; serves whatever the fetcher writes, **no
   restart needed** when the tree updates.
2. `gopher-cta-fetcher` — the fetcher, `--env-file .env -v <repo>/public:/srv
   --interval 30`, regenerates the tree (live trains + narration) every 30s.

To redeploy a code change: `felipe` pushes the image (keychain), then
```sh
docker rm -f gopher-cta-fetcher
docker run -d --name gopher-cta-fetcher --env-file .env \
  -v /Users/felipe/Projects/gopher-cta/public:/srv \
  ghcr.io/felipedbene/gopher-cta:latest --interval 30 --out /srv
```
(geomyidae keeps running.) Image is amd64; runs under emulation on the arm Mac —
fine, but an arm64 build would be leaner for local. If `public/current` is
missing, no fetcher is running — that's the usual "0 bytes from gopher" cause.

## Conventions

- **One commit per task item**, no monster bundles. Branch off `master` for
  feature work.
- **No premature abstraction**: concrete types until a 2nd impl justifies a
  trait (the reference `geo.rs`'s `Project`/`Grid` traits were intentionally
  dropped). The reference `geo.rs` and `cc_prompt_gopher_geo.md` at repo root are
  scratch inputs, untracked — `src/atlas.rs` is the real implementation.
- felipe wants a proposal/review checkpoint before large implementation work.
- Sibling repos: `~/Projects/cta-tui` (CTA wire + AI Worker contract source of
  truth), `~/Projects/bbs`.

## Status (update as it changes)

geo atlas (commit 1) + AI narration pages (commit 2) merged to `master`;
`ghcr.io/felipedbene/gopher-cta:latest` built+pushed and **deployed locally** —
fetcher + geomyidae containers serving `gopher://10.0.10.69:7070`; dispatch /
sitrep / events validated live. Commit 3 (landmarks type-1 menu + detail pages,
ANSI map variant) not started.
