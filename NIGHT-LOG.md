# NIGHT-LOG — gopher-cta

Overnight unsupervised build. Scan this in two minutes; details in README.

## 2026-06-27 — hub cross-link to gopher-blog

Added the one-line hub front-door link from cta (`:70`) to the new phlog hole
(gopher-blog, `:7071`). `Entry::Link` grew `host`/`port` (`None` → geomyidae's own
placeholders, byte-identical to before; `Some` → a concrete address the client
dials directly); `link_remote()` constructs cross-server links; `render_menu_index`
emits the new columns. `root_menu` gains one `Phlog -- the blog` type-1 entry,
host/port from `--phlog-link` (default `gopher://gopher.debene.dev:7071`, `none`
disables). No proxy, no docroot merge — purely a menu link to a sibling port. A
golden test asserts the phlog build differs from the no-phlog build by exactly
that one line; all existing links stay byte-identical. `Entry`'s new shape is kept
in sync with the copy in gopher-blog (the pre-`gopher-core` API). 58 tests + clippy
`-D warnings` + fmt green.

## TL;DR

Done and working. A Gopher server in Rust that plots live CTA 'L' trains as a
Braille map. Boots offline from a bundled fixture (no key needed), serves a root
menu, a braille `/map`, a `/cta` line menu, and `/cta/<line>` text. All tests +
clippy + fmt green. Verified end-to-end with `nc` and `curl gopher://`.

## What got built

- **`braille.rs`** — `Canvas::new(wc,hc)` / `set(px,py)` / `render()`. 2×4 dot
  glyphs, base U+2800. Spec test vectors asserted: `(0,0)→0x2801 ⠁`,
  all-8→`0x28FF ⣿`, `(0,0)+(1,3)→0x2881 ⢁`. Empty cells render as blank-braille
  U+2800 (not space) so width is stable in monospaced clients.
- **`project.rs`** — linear lat/lon→pixel with `cos(lat_mid)` longitude aspect
  correction. Derived geometry: WP=160 (80 cells) → HP≈237 → HC=60 cells.
  Out-of-bbox points dropped, not clamped. North up (y flipped). Tests assert
  the four bbox corners map to the four canvas corners and centre→centre.
- **`protocol.rs`** — `ItemType`, `MenuItem`, `render_menu`, `render_text`,
  `parse_selector`, `split_query`. CRLF on every line; menus/text terminated by
  a lone `.\r\n`. `render_text` escapes a body line that is exactly `.` → `..`
  (RFC 1436). Tests assert tab counts, CRLF endings, and the terminator.
- **`transit.rs`** — `TransitSource` trait, `Train`, `Positions`, `CtaSource`
  (live Train Tracker fetch with fixture fallback), `MetraSource` stub. Wire
  parsing (incl. the `OneOrMany` "one or many" quirk) mirrors `cta-tui/src/cta.rs`.
- **`server.rs`** — tokio accept loop (one selector per conn, then close) +
  selector routing + the **pure** view builders (`build_root`, `build_map`,
  `build_cta_menu`, `build_line_text`), which are unit-tested without a socket.
- **`main.rs`** — env-var config (`CTA_TRAIN_API_KEY`, `CTA_ROUTES`,
  `GOPHER_PORT`, `GOPHER_HOST`), fixture compiled in via `include_str!`.
- **`fixtures/positions.json`** — hand-built recorded snapshot: 18 trains across
  all 8 lines, with real-ish Chicago station coordinates. The `y` route is a
  single object (not array) on purpose, to exercise the `OneOrMany` path.

## Verification (what I actually ran)

- `cargo test` → **31 passed**.
- `cargo clippy --all-targets -- -D warnings` → clean.
- `cargo fmt --check` → clean.
- Booted the server (no key) and hit it live:
  - root menu: tab-delimited `i`/`0`/`1` lines, ends with `.\r\n`. ✓
  - `/map`: 60 braille rows, "18 trains plotted of 18 reporting", per-line
    legend, feed timestamp. Dots trail north–south down the centre (the
    State St / Red Line spine) — a recognizable Chicago spread. ✓
  - `/cta`: 8 lines with live counts. ✓  `/cta/red`: 5 trains listed. ✓
  - `/cta/y` (single-train route): renders 1 train → `OneOrMany` works live. ✓
  - Unknown selector → friendly text, still `.`-terminated. ✓
  - `curl gopher://localhost:PORT/`, `/0/map`, `/0/cta/red`, `/1/cta` all OK.

## Decisions / things worth knowing

- **Env var name:** brief says `CTA_TRAIN_API_KEY`; cta-tui uses `CTA_KEY`. I
  followed the brief (`CTA_TRAIN_API_KEY`). If you want it to share cta-tui's
  `.env`, rename in `main.rs` or set both.
- **Live-fetch failure falls back to the fixture** (logged to stderr), so the
  map never blanks. If you'd rather it surface the error to the client instead,
  that's a one-line change in `CtaSource::positions`.
- **`TransitSource` uses native async-fn-in-trait** desugared to
  `impl Future + Send` (needed so the source can be driven under `tokio::spawn`).
  No `async_trait` dep. Static dispatch only — the server holds a concrete
  `CtaSource`, consistent with "no premature abstraction": the trait exists
  because the brief mandated the Metra seam, not to over-generalize.
- **Map is tall (60 rows).** That's the honest aspect ratio of the bbox at
  WP=160. With only 18 fixture trains it looks sparse; live (hundreds of trains)
  it fills in. Tighten the bbox or lower WP in `project.rs` for a denser/smaller
  plot — all knobs are labelled TUNABLE there.
- **Fixture coordinates** are realistic but hand-placed (I had no API key to
  record a real snapshot). They sit correctly on the lines geographically.

## Stubbed / not done (by design — scope guards)

- **Metra / GTFS-RT:** `MetraSource` returns empty. Not wired into `main`.
  `TODO(felipe)` in `transit.rs` marks the implementation point.
- **No TLS/gophers, no auth/sessions, no caching, no config files, no HTTP/web
  frontend, no deploy.** As specified.

## Stretch goals NOT taken (skipped on friction-avoidance, all optional)

- Per-station arrivals, **type-7 station search** (the `Search` item type and
  `split_query` exist and are tested — the seam is there, just not routed),
  per-line braille view, run-number labels on the map, route-shape overlay.

## Every TODO(felipe) in the tree

- `transit.rs` — `MetraSource::positions`: implement Metra via the GTFS-RT
  vehicle-positions feed.

## Commits

1. `gopher-cta: braille CTA map server (modules, tests, fixture)`
2. `docs: README + NIGHT-LOG` (this commit)
