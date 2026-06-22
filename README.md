# gopher-cta

A small, self-contained **Gopher (RFC 1436) server** that serves **live CTA 'L'
train positions as a geographic map rendered with Unicode Braille**, plus
per-line text positions. Written in Rust (async via `tokio`), minimal deps.

```
                CTA Train Tracker  ──► CtaSource ──► braille canvas ──► Gopher
                (or bundled fixture)                  (lat/lon → dots)
```

## What it serves

| Selector       | Type | Content                                                        |
| -------------- | ---- | -------------------------------------------------------------- |
| `` (root)      | menu | Links to the map, the per-line menu, and about.                |
| `/map`         | text | **Braille geographic plot of every live train** + legend + feed time. |
| `/cta`         | menu | One entry per 'L' line, with a live train count.               |
| `/cta/<line>`  | text | That line's trains: run, destination, next stop, heading, coords. |
| `/about`       | text | What this is and the canvas/projection parameters.             |

Line keys: `red blue brn g org p pink y`.

## Running

```sh
cargo run            # boots on :7070, serving the bundled fixture (no key needed)
```

Point any gopher client at it:

```sh
# raw TCP — selector terminated by CRLF, server replies then closes
printf '/map\r\n' | nc localhost 7070

# curl speaks gopher://
curl gopher://localhost:7070/          # root menu
curl gopher://localhost:7070/0/map     # the braille map (0/ = "this is a text item")
curl gopher://localhost:7070/0/cta/red

# or a real client
lynx gopher://localhost:7070
```

The map is best viewed in a terminal with a font that has Braille glyphs
(U+2800–U+28FF) — most monospaced fonts do.

### Configuration (environment variables only)

| Var                 | Default                        | Meaning                                            |
| ------------------- | ------------------------------ | -------------------------------------------------- |
| `CTA_TRAIN_API_KEY` | _unset_                        | Train Tracker API key. **Unset ⇒ offline fixture mode.** |
| `CTA_ROUTES`        | `red,blue,brn,g,org,p,pink,y`  | Comma-separated route keys to fetch.               |
| `GOPHER_PORT`       | `7070`                         | TCP listen port (non-privileged).                  |
| `GOPHER_HOST`       | `localhost`                    | Host advertised in menu item links.                |

The server **always boots**: with no key (or if a live fetch fails) it serves
the recorded snapshot in `fixtures/positions.json`, so the whole thing is
demoable and testable offline. Get a free key at
<https://www.transitchicago.com/developers/traintracker/>.

```sh
CTA_TRAIN_API_KEY=xxxx GOPHER_PORT=7070 cargo run   # live data
```

A local `.env` (gitignored) is loaded at startup, so you can drop the key in a
file instead of exporting it. A real exported env var still takes precedence:

```sh
# .env
CTA_TRAIN_API_KEY=your-train-tracker-key-here
```

## How the braille map works

Each Braille glyph (base codepoint **U+2800**) is a 2-wide × 4-tall grid of 8
dots, so a canvas of `Wc × Hc` characters holds `(2·Wc) × (4·Hc)` plottable
pixels. Setting a pixel ORs its dot's bit into that cell's byte. See
`src/braille.rs` for the dot→bit map and the spec test vectors
(`(0,0)→⠁`, all-dots→`⣿`, `(0,0)+(1,3)→⢁`).

### Projection and the bounding box (tunable)

`src/project.rs` maps lat/lon linearly onto the canvas, with a longitude aspect
correction by `cos(lat_mid)` (full Mercator is overkill at city scale). All the
knobs live at the top of that file and are labelled **TUNABLE**:

```rust
pub const LAT_MIN: f64 = 41.65;   // south edge
pub const LAT_MAX: f64 = 42.07;   // north edge
pub const LON_MIN: f64 = -87.90;  // west edge
pub const LON_MAX: f64 = -87.52;  // east edge
pub const WP: usize    = 160;     // canvas pixel width (= 80 braille cells); height is derived
```

- **Zoom in** on downtown: tighten the bbox (e.g. `LAT_MIN=41.85, LAT_MAX=41.95`).
- **Include more suburbs**: widen it. Height auto-derives to preserve aspect.
- **Bigger/smaller canvas**: change `WP` (keep it even).

Points outside the bbox are **dropped**, not clamped, so off-map trains vanish
rather than smearing along the edges. North is up (y is flipped).

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
  project.rs   bbox + cos(lat) projection. Pure, unit-tested (corners→corners).
  protocol.rs  gopher menu/text builders, CRLF + trailing-dot, selector parse.
  transit.rs   TransitSource trait, Train, CtaSource (live+fixture), MetraSource stub.
  server.rs    tokio accept loop, selector routing, pure view builders.
  main.rs      env-var config + wiring.
fixtures/
  positions.json   recorded ttpositions snapshot (offline demo + tests).
```

The CTA wire-parsing layer (the `OneOrMany` "one or many" handling that CTA's
XML→JSON conversion forces) mirrors the working code in the sibling `cta-tui`
repo, which is the source of truth for field names.

## Testing

```sh
cargo test                       # 31 unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Covered: braille bit-mapping (spec vectors), projection (bbox corners land at
canvas corners, midpoint near center, out-of-bbox dropped), protocol (tabs
present, CRLF endings, menu ends with `.`, type-7 selector splits on TAB), and
an end-to-end fixture render of `/map` (non-empty, correct row count, contains
braille glyphs).

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

```sh
scp dist/gopher-cta-powerpc64 you@g5:~/
# on the G5:
CTA_TRAIN_API_KEY=... GOPHER_HOST=<g5-lan-ip> ./gopher-cta-powerpc64
```

It listens on `0.0.0.0:7070`, so point any gopher client on your LAN at the G5.
Without a key it serves the bundled fixture (no network/TLS needed at all).
Verified end-to-end under emulation: the big-endian binary boots, fetches live
CTA data over HTTPS (OpenSSL), and serves the braille map and drill-down menus.

## Scope

CTA only this build. No Metra/GTFS-RT (stub), no auth, no HTTP frontend. TLS is
used only for the outbound CTA fetch (rustls by default, OpenSSL for big-endian
targets). Not affiliated with the Chicago Transit Authority.
