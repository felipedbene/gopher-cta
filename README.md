# gopher-cta

A **fetcher** that turns **live CTA 'L' train positions into a static gopher
tree** — a geographic map rendered with Unicode Braille, per-line listings, and
per-train detail pages — for an existing gopher daemon (**geomyidae**) to serve.
Written in Rust, minimal deps. No protocol server of its own.

```
  CTA Train Tracker ─► CtaSource ─► render (braille map, menus, pages)
  (or bundled fixture)                     │
                                           ▼  atomic publish
                              <out>/out-<ts>/ … ──► flip <out>/current symlink
                                           │
                                  geomyidae serves <out>/current
```

## Quickstart (local, with Docker)

Clone → browsing, no native geomyidae needed. Fixture mode (no key); add
`-e CTA_TRAIN_API_KEY=<key>` for live data.

```sh
# 1. render the gopher tree into ./public
docker build -t gopher-cta:local .
docker run --rm -e CTA_TRAIN_API_KEY= -v "$PWD/public":/srv gopher-cta:local --once --out /srv

# 2. serve it with geomyidae
docker build -t geomyidae:local -f deploy/Dockerfile.geomyidae deploy
docker run --rm -d --name geo -p 7070:7070 -v "$PWD/public":/srv:ro geomyidae:local

# 3. consume it
printf '/red\r\n' | nc localhost 7070        # per-line drill-down
curl gopher://localhost:7070/0/map.txt       # the braille map
lynx gopher://localhost:7070                  # browse interactively

docker rm -f geo                              # stop the daemon
```

Already have `geomyidae` installed? Skip Docker: `cargo run -- --once --out ./public`
then `geomyidae -b ./public/current -p 7070`. Full options below.

## The published tree

The fetcher writes this under each snapshot (the daemon serves `current/`):

| Path                | Type | Content                                                       |
| ------------------- | ---- | ------------------------------------------------------------- |
| `index.gph`         | menu | Root: link to the map + one entry per line (with live counts).|
| `map.txt`           | text | **Braille geographic plot of every live train** + legend + feed time. |
| `<line>/index.gph`  | menu | That line's running trains; each drills into a detail page.   |
| `train/<run>.txt`   | text | One train: line/color, position+heading, next stop + predicted time. |
| `about.txt`         | text | What this is and the canvas/projection parameters.            |

Line keys: `red blue brn g org p pink y`. Menus are geomyidae `.gph` files; the
`server`/`port` fields are left as placeholder tokens that geomyidae fills in,
so the tree is host/port-agnostic.

## Running

```sh
# render the tree once into ./public (bundled fixture, no key needed)
cargo run -- --once --out ./public

# or loop, refreshing every 30s
cargo run -- --interval 30 --out ./public
```

Then point geomyidae at the published snapshot and browse:

```sh
geomyidae -b ./public/current -p 7070
curl gopher://localhost:7070/             # root menu
curl gopher://localhost:7070/0/map.txt    # the braille map (0/ = text item)
lynx gopher://localhost:7070
```

The map is best viewed in a terminal with a font that has Braille glyphs
(U+2800–U+28FF) — most monospaced fonts do.

### CLI flags

| Flag               | Default | Meaning                                            |
| ------------------ | ------- | -------------------------------------------------- |
| `--once`           | off     | Render one snapshot and exit (else loop).          |
| `--interval <secs>`| `30`    | Loop refresh interval.                             |
| `--out <dir>`      | `public`| Output dir; daemon serves `<dir>/current`. (`$GOPHER_OUT`) |

### Configuration (environment variables)

| Var                 | Default                        | Meaning                                            |
| ------------------- | ------------------------------ | -------------------------------------------------- |
| `CTA_TRAIN_API_KEY` | _unset_                        | Train Tracker API key. **Unset ⇒ offline fixture mode.** |
| `CTA_ROUTES`        | `red,blue,brn,g,org,p,pink,y`  | Comma-separated route keys to fetch.               |
| `GOPHER_OUT`        | `public`                       | Output dir (same as `--out`).                      |

The fetcher **always produces a tree**: with no key (or if a live fetch fails)
it renders the recorded snapshot in `fixtures/positions.json`, so the whole
thing is demoable and testable offline. Get a free key at
<https://www.transitchicago.com/developers/traintracker/>.

```sh
CTA_TRAIN_API_KEY=xxxx cargo run -- --interval 30 --out ./public   # live data
```

A local `.env` (gitignored) is loaded at startup, so you can drop the key in a
file instead of exporting it. A real exported env var still takes precedence:

```sh
# .env
CTA_TRAIN_API_KEY=your-train-tracker-key-here
```

### Why a fetcher + daemon (not a custom server)

Rendering to static files served by a hardened, battle-tested gopher daemon
means no bespoke socket/selector code to maintain, atomic snapshot publishing
(readers never see a half-written tree), and trivial horizontal serving. The
daemon-specific bit is one function, `render_menu_index()` in `src/fetch.rs`;
switching to e.g. Gophernicus means rewriting only that.

## How the braille map works

Each Braille glyph (base codepoint **U+2800**) is a 2-wide × 4-tall grid of 8
dots, so a canvas of `Wc × Hc` characters holds `(2·Wc) × (4·Hc)` plottable
pixels. Setting a pixel ORs its dot's bit into that cell's byte. See
`src/braille.rs` for the dot→bit map and the spec test vectors
(`(0,0)→⠁`, all-dots→`⣿`, `(0,0)+(1,3)→⢁`).

### Projection and the bounding box (tunable)

`src/project.rs` maps lat/lon onto the canvas with a km-based model: longitude is
shrunk by `cos(lat_c)` and the row budget is derived from the column budget and
`CELL_ASPECT` so the city renders north-up and undistorted (full Mercator is
overkill at city scale). All the knobs live at the top of that file, labelled
**TUNABLE**:

```rust
pub const LAT_MIN: f64 = 41.65;       // south edge
pub const LAT_MAX: f64 = 42.07;       // north edge
pub const LON_MIN: f64 = -87.90;      // west edge
pub const LON_MAX: f64 = -87.48;      // east edge (past the shore into open lake)
pub const W: usize          = 48;     // column budget (braille cells); rows derived (~36)
pub const CELL_ASPECT: f64  = 2.0;    // terminal cell height/width
pub const LAT_KM_PER_DEG: f64 = 111.32;
```

- **Zoom in** on downtown: tighten the bbox (e.g. `LAT_MIN=41.85, LAT_MAX=41.95`).
- **Include more suburbs**: widen it. Rows auto-derive to preserve aspect.
- **Bigger/smaller canvas**: change `W`.

Points outside the bbox are **dropped**, not clamped (off-map trains vanish
rather than smearing along the edges); dropped run ids are logged at debug level.
North is up (the row axis is flipped).

## Extending to other agencies (the Metra seam)

`src/transit.rs` defines a `TransitSource` trait:

```rust
pub trait TransitSource {
    fn name(&self) -> &str;
    fn positions(&self) -> impl Future<Output = Result<Positions, BoxErr>> + Send;
}
```

`CtaSource` is the real implementation. `MetraSource` is a **stub** that returns
no trains — the extension point is real but unbuilt this round (no GTFS-RT). To
add Metra: implement `positions()` against the Metra GTFS-realtime
vehicle-positions feed, map each vehicle into a `Train { lat, lon, line, .. }`,
and the existing braille map will plot it with no other changes (you'd widen the
bbox to cover the regional rail footprint). Look for `TODO(felipe)` in that file.

## Project layout

```
src/
  braille.rs   2×4 dot canvas; set(px,py); render() → String. Pure, unit-tested.
  project.rs   km-based bbox projection. Pure, unit-tested (corners→corners).
  render.rs    pure render core: text pages + daemon-agnostic menu model (Entry).
  transit.rs   TransitSource trait, Train, CtaSource (live+fixture), MetraSource stub.
  fetch.rs     fetch loop, atomic publish (current symlink + GC), geomyidae .gph.
  main.rs      env/flag config + wiring.
fixtures/
  positions.json   recorded ttpositions snapshot (offline demo + tests).
```

`render.rs` is the pure, daemon-agnostic core (feed data → strings + `Entry`
menus); `fetch.rs` is the only place that knows about geomyidae (the
`render_menu_index()` `.gph` serializer) and the filesystem. The CTA
wire-parsing layer (the `OneOrMany` "one or many" handling that CTA's XML→JSON
conversion forces) mirrors the sibling `cta-tui` repo, the source of truth for
field names.

## Testing

```sh
cargo test                       # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Covered: braille bit-mapping (spec vectors), projection (bbox corners land at
canvas corners, known lat/lon → expected cell, out-of-bbox dropped), render
(known feed → expected listing/links, map braille+footer, train detail
valid/unknown), and the fetcher (atomic publish + `current` symlink, GC, the
geomyidae `.gph` serialization, full-tree build).

## Cross-compiling for PowerPC (PowerMac G5)

The G5 (PowerPC 970) is **big-endian** `powerpc64-unknown-linux-gnu` — *not*
`powerpc64le` (that's little-endian POWER8+). One snag: the default TLS stack
(rustls → **ring**) has no big-endian support, so the default build can't target
the G5. TLS is only needed for the outbound CTA HTTPS fetch, so the fix is a
Cargo feature that swaps the backend to OpenSSL:

| Feature      | TLS backend            | Use for                                  |
| ------------ | ---------------------- | ---------------------------------------- |
| `tls-rustls` | rustls + ring (default)| x86/arm/macOS — your normal `cargo build`|
| `tls-native` | OpenSSL (vendored)     | big-endian targets like the G5           |

The big-endian ppc64 toolchain isn't in Debian/Ubuntu's normal repos (only
little-endian `ppc64el` is), so the build runs inside the `cross-rs` toolchain
image with Rust installed on top. One command:

```sh
./scripts/build-ppc64.sh      # -> dist/gopher-cta-powerpc64
```

Requires Docker. On Apple Silicon it builds under emulation (slow but correct).
The result is a big-endian **Power ELF V1** binary with OpenSSL statically
vendored in, so it only needs glibc at runtime:

```
ELF 64-bit MSB pie executable, 64-bit PowerPC, Power ELF V1 ABI ... dynamically linked
NEEDED: libgcc_s, librt, libpthread, libm, libdl, libc   (no libssl/libcrypto)
```

**glibc caveat:** built against the toolchain image's glibc (2.31). It'll run on
a G5 distro with glibc ≥ 2.31 (current Debian ppc64 ports, Adélie, Void PPC,
recent Gentoo). Check on the G5 with `ldd --version`; if it's older, rebuild on
an older base image or switch to the `powerpc64-unknown-linux-musl` target.

### Running it on the G5

The G5 runs the **fetcher** (writes the tree); a gopher daemon on the same box
serves it. Install geomyidae from your distro (or build from bitreich source),
then:

```sh
scp dist/gopher-cta-powerpc64 you@g5:~/
# on the G5:
CTA_TRAIN_API_KEY=... ./gopher-cta-powerpc64 --interval 30 --out ~/public &
geomyidae -b ~/public/current -p 7070
```

Point any gopher client on your LAN at the G5. Without a key the fetcher renders
the bundled fixture (no network/TLS needed at all). Verified under emulation:
the big-endian binary fetches live CTA data over HTTPS (OpenSSL) and writes the
full braille-map + drill-down tree.

## Container / Kubernetes

The fetcher–daemon split maps cleanly onto a single pod with two containers
sharing an `emptyDir`: the **fetcher** writes the tree to `/srv` and flips
`/srv/current`; **geomyidae** serves `/srv/current` read-only. No PVC — the tree
is regenerated every interval, so ephemeral storage is the right fit.

```
Dockerfile                    fetcher image (Rust build → debian-slim)
deploy/Dockerfile.geomyidae   geomyidae image (built from bitreich source)
deploy/gopher-cta.yaml        Secret + Deployment (2 containers, emptyDir) + Service
```

```sh
docker build -t ghcr.io/<you>/gopher-cta:latest .
docker build -t ghcr.io/<you>/geomyidae:latest -f deploy/Dockerfile.geomyidae deploy
# edit image names + the geomyidae -h <host> + the CTA key in the Secret, then:
kubectl apply -f deploy/gopher-cta.yaml
```

Both containers run unprivileged (`nobody`, dropped caps, read-only rootfs); an
`fsGroup` makes the shared `emptyDir` writable for the fetcher. The Service maps
gopher's port 70 to the unprivileged container port 7070. Set geomyidae's
`-h <host>` to the address clients reach it on so the `.gph` `server` placeholder
resolves correctly.

## Scope

CTA only this build. No Metra/GTFS-RT (stub), no auth, no HTTP frontend, no
gopher protocol server of our own (a daemon serves the static tree). No type-7
search — navigation + drill-down only. TLS is used only for the outbound CTA
fetch (rustls by default, OpenSSL for big-endian targets). Not affiliated with
the Chicago Transit Authority.
