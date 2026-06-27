//! Fetcher front end: pull the CTA feed and publish a static gopher tree that an
//! external daemon (geomyidae) serves. No sockets of our own — we render files.
//!
//! Publishing is **atomic**: each cycle renders into a fresh `out-<ts>/` snapshot
//! directory, then flips a `current` symlink to it with an atomic rename. The
//! daemon is pointed at `current/`, so a reader always sees a complete tree, never
//! a half-written one. Old snapshots are garbage-collected.
//!
//! This module owns the process loop and the publish mechanism; the tree's
//! contents (which files, the menu format) live in [`crate::render`] and the
//! tree writer below.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::atlas::Atlas;
use crate::narration::{self, NarrationView};
use crate::project::{self, Geometry};
use crate::render;
use crate::transit::{Positions, TransitSource};

/// Published snapshots to retain (besides whatever `current` resolves to).
const KEEP_SNAPSHOTS: usize = 3;

/// The GopherII caps policy file, embedded byte-for-byte so its CRLF line
/// endings are preserved exactly (`include_bytes!`, not `include_str!`). Served
/// verbatim by geomyidae at the `caps.txt` selector.
const CAPS_TXT: &[u8] = include_bytes!("../caps.txt");

/// Crawler policy. Disallows the ephemeral `/train/` detail pages (churning run
/// numbers) while leaving the stable-selector tree indexable. Served at the
/// `robots.txt` selector; written into every snapshot so it survives republish.
const ROBOTS_TXT: &[u8] = include_bytes!("../robots.txt");

/// Default path of the source tarball baked into the image (see `Dockerfile`).
/// Overridable with `GOPHER_SRC_ARCHIVE`. Served over gopher as `/src.tar.gz`
/// when present; a bare `cargo run` (no image) simply omits the link. NOTE: the
/// archive deliberately excludes the MaxMind `.mmdb` (not redistributable).
const SRC_ARCHIVE_PATH: &str = "/usr/local/share/gopher-cta/src.tar.gz";

/// Load the served source tarball, if one was baked into the image. `None` (no
/// `/src.tar.gz`, no menu link) when the file is absent or unreadable.
fn load_src_archive() -> Option<Vec<u8>> {
    let path = std::env::var("GOPHER_SRC_ARCHIVE").unwrap_or_else(|_| SRC_ARCHIVE_PATH.into());
    fs::read(&path).ok()
}

/// Default hub link to the sibling phlog hole (gopher-blog), advertised in the
/// root menu. Overridable with `--phlog-link`; `none` disables it.
const DEFAULT_PHLOG_LINK: &str = "gopher://gopher.debene.dev:7071";

/// Fetcher configuration, parsed from CLI args / env.
pub struct Config {
    pub out: PathBuf,
    pub once: bool,
    pub interval: Duration,
    /// Hub link to the phlog: `(host, port)`, or `None` to omit the root entry.
    pub phlog_link: Option<(String, u16)>,
}

impl Config {
    /// Parse args after the `fetch` subcommand. `--once`, `--interval <secs>`
    /// (default 30), `--out <dir>` (default `$GOPHER_OUT` or `public`),
    /// `--phlog-link <gopher://host[:port]|none>` (default `DEFAULT_PHLOG_LINK`).
    pub fn from_args(args: &[String]) -> Result<Config, String> {
        let mut out =
            PathBuf::from(std::env::var("GOPHER_OUT").unwrap_or_else(|_| "public".into()));
        let mut once = false;
        let mut interval = Duration::from_secs(30);
        let mut phlog_raw = DEFAULT_PHLOG_LINK.to_string();

        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--once" => once = true,
                "--interval" => {
                    let v = it.next().ok_or("--interval needs <secs>")?;
                    let secs: u64 = v.parse().map_err(|_| format!("bad --interval: {v}"))?;
                    interval = Duration::from_secs(secs.max(1));
                }
                "--out" => out = PathBuf::from(it.next().ok_or("--out needs <dir>")?),
                "--phlog-link" => phlog_raw = it.next().ok_or("--phlog-link needs <url>")?.clone(),
                other => return Err(format!("unknown fetch arg: {other}")),
            }
        }
        Ok(Config {
            out,
            once,
            interval,
            phlog_link: parse_phlog_link(&phlog_raw)?,
        })
    }
}

/// Parse `--phlog-link`: a `gopher://host[:port]` URL into `(host, port)` (port
/// defaults to 70). The literal `none` (or empty) disables the link.
fn parse_phlog_link(raw: &str) -> Result<Option<(String, u16)>, String> {
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    let rest = raw
        .strip_prefix("gopher://")
        .ok_or_else(|| format!("--phlog-link must start with gopher:// (got {raw})"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| format!("bad port in --phlog-link: {p}"))?,
        ),
        None => (authority.to_string(), 70u16),
    };
    if host.is_empty() {
        return Err(format!("--phlog-link has no host: {raw}"));
    }
    Ok(Some((host, port)))
}

/// Run the fetcher: fetch -> publish, once or on an interval.
pub async fn run<S: TransitSource>(cfg: Config, source: S) -> io::Result<()> {
    let geo = project::geometry();
    // Rasterize the static geo overlay once; each publish clones it (never
    // re-rasterizes) and paints live trains on top.
    let atlas = Atlas::build(geo);
    // Same idea for the braille map's ANSI overlay: the Chicago skeleton
    // (coast + river + expressways) rasterized once, cloned per publish.
    let map_base = render::MapBase::build(&geo);
    // AI narrative panels poll the Worker on a slow cadence in the background;
    // each publish reads a snapshot and never blocks on (or depends on) them.
    let narration = narration::spawn();
    // Source tarball baked into the image (Dockerfile), or None for a bare run.
    let src_archive = load_src_archive();
    if src_archive.is_some() {
        eprintln!("[fetch] source archive present -> serving /src.tar.gz");
    }
    eprintln!(
        "[fetch] out={} mode={}",
        cfg.out.display(),
        if cfg.once {
            "once".to_string()
        } else {
            format!("every {}s", cfg.interval.as_secs())
        }
    );
    loop {
        match source.positions().await {
            Ok(pos) => {
                // Snapshot the latest narration without blocking the train path.
                let view = narration.lock().unwrap().clone();
                let phlog = cfg.phlog_link.as_ref().map(|(h, p)| (h.as_str(), *p));
                match publish(
                    &cfg.out,
                    &pos,
                    &geo,
                    &atlas,
                    &map_base,
                    &view,
                    source.name(),
                    src_archive.as_deref(),
                    phlog,
                ) {
                    Ok(snap) => eprintln!(
                        "[fetch] published {} ({} trains) -> {}/current",
                        snap.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                        pos.trains.len(),
                        cfg.out.display()
                    ),
                    Err(e) => eprintln!("[fetch] publish failed: {e}"),
                }
            }
            Err(e) => eprintln!("[fetch] fetch failed: {e}"),
        }
        if cfg.once {
            break;
        }
        tokio::time::sleep(cfg.interval).await;
    }
    Ok(())
}

/// Render the tree into a fresh `out-<ts>/`, then atomically flip `current` and
/// GC old snapshots. Returns the snapshot directory.
#[allow(clippy::too_many_arguments)] // render plumbing; each input is distinct
fn publish(
    out: &Path,
    pos: &Positions,
    geo: &Geometry,
    atlas: &Atlas,
    map_base: &render::MapBase,
    narration: &NarrationView,
    source_name: &str,
    src_archive: Option<&[u8]>,
    phlog: Option<(&str, u16)>,
) -> io::Result<PathBuf> {
    fs::create_dir_all(out)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| io::Error::other(e.to_string()))?
        .as_nanos();
    let snap = out.join(format!("out-{ts}"));
    fs::create_dir_all(&snap)?;
    write_tree(
        &snap,
        pos,
        geo,
        atlas,
        map_base,
        narration,
        source_name,
        src_archive,
        phlog,
    )?;
    gopher_core::flip_current(out, &snap)?;
    gopher_core::gc(out, KEEP_SNAPSHOTS)?;
    Ok(snap)
}

/// Write the full navigable gopher tree for one snapshot:
///   index.gph            root menu (map link + one entry per line)
///   map.txt              braille map (text)
///   map.ansi             braille map, ANSI-coloured by line
///   atlas.txt            char-cell geographic atlas: coast + landmarks + trains
///   atlas.ansi           atlas, ANSI-coloured (trains by line)
///   dispatch.txt         AI dispatch summary + live feed stats
///   sitrep.txt           AI SITREP (alerts summary) for the home station
///   events.txt           AI event advisory
///   about.txt            about page (text, with ASCII masthead)
///   faq.txt              FAQ (rendering questions)
///   help.txt             troubleshooting (actionable fixes)
///   dig.txt              hidden easter egg — written but linked from no menu
///   src.tar.gz           full source tarball (type 9), when baked into the image
///   caps.txt             GopherII caps policy file (verbatim, CRLF)
///   robots.txt           crawler policy (disallows ephemeral /train/ pages)
///   <line>/index.gph     per-line menu, each train a drill-down link
///   train/<run>.txt      per-train detail page (text)
///   landmarks/index.gph  type-1 menu of Chicago landmarks
///   landmark/<X>.txt     per-landmark detail page (keyed by marker letter)
#[allow(clippy::too_many_arguments)] // render plumbing; each input is distinct
fn write_tree(
    dir: &Path,
    pos: &Positions,
    geo: &Geometry,
    atlas: &Atlas,
    map_base: &render::MapBase,
    narration: &NarrationView,
    source_name: &str,
    src_archive: Option<&[u8]>,
    phlog: Option<(&str, u16)>,
) -> io::Result<()> {
    // Root menu + top-level text pages. Advertise the source tarball only when
    // it's actually written into this snapshot.
    fs::write(
        dir.join("index.gph"),
        gopher_core::render_menu_index(&render::root_menu(pos, src_archive.is_some(), phlog)),
    )?;
    if let Some(bytes) = src_archive {
        fs::write(dir.join("src.tar.gz"), bytes)?;
    }
    fs::write(dir.join("map.txt"), render::map_page(pos, geo, source_name))?;
    fs::write(
        dir.join("map.ansi"),
        render::map_page_ansi(map_base, pos, geo, source_name),
    )?;
    fs::write(dir.join("atlas.txt"), atlas.render(pos, source_name))?;
    fs::write(dir.join("atlas.ansi"), atlas.render_ansi(pos, source_name))?;
    // Narrative panels (last-good snapshot; renders placeholders if never
    // retrieved). `now` once so all three pages share a consistent age.
    let now = narration::now_secs();
    fs::write(
        dir.join("dispatch.txt"),
        narration::dispatch_page(narration, pos, now),
    )?;
    fs::write(
        dir.join("sitrep.txt"),
        narration::sitrep_page(narration, now),
    )?;
    fs::write(
        dir.join("events.txt"),
        narration::events_page(narration, now),
    )?;
    fs::write(dir.join("about.txt"), render::about_page())?;
    fs::write(dir.join("faq.txt"), render::faq_page())?;
    fs::write(dir.join("help.txt"), render::help_page())?;
    // Hidden easter egg: written into every snapshot but linked from no menu —
    // reachable only by guessing the selector (the FAQ drops a hint).
    fs::write(dir.join("dig.txt"), render::dig_page())?;
    // GopherII/Floodgap caps policy file. Written byte-for-byte (CRLF preserved)
    // into every snapshot so it survives the atomic republish — geomyidae serves
    // it verbatim at the `caps.txt` selector. Authored content, not generated.
    fs::write(dir.join("caps.txt"), CAPS_TXT)?;
    // Crawler policy (keeps indexers off the ephemeral /train/ pages). Likewise
    // written into every snapshot so it persists across republish.
    fs::write(dir.join("robots.txt"), ROBOTS_TXT)?;

    // One submenu directory per line (its index.gph is the per-line listing).
    for &line in render::LINE_ORDER {
        let ldir = dir.join(line);
        fs::create_dir_all(&ldir)?;
        fs::write(
            ldir.join("index.gph"),
            gopher_core::render_menu_index(&render::line_menu(pos, line)),
        )?;
    }

    // One detail page per running train.
    let tdir = dir.join("train");
    fs::create_dir_all(&tdir)?;
    for t in &pos.trains {
        // Run ids are numeric in the feed; guard against anything that could
        // escape the tree before using it as a filename.
        if !t.run.chars().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        fs::write(
            tdir.join(format!("{}.txt", t.run)),
            render::train_page(pos, &t.run),
        )?;
    }

    // Landmarks: a type-1 menu (landmarks/index.gph) + one detail page each.
    let lmdir = dir.join("landmarks");
    fs::create_dir_all(&lmdir)?;
    fs::write(
        lmdir.join("index.gph"),
        gopher_core::render_menu_index(&atlas.landmarks_menu()),
    )?;
    let mdir = dir.join("landmark");
    fs::create_dir_all(&mdir)?;
    for (marker, page) in atlas.landmark_pages() {
        // Markers are A-Z (filename-safe); guard anyway before using as a path.
        if marker.is_ascii_alphanumeric() {
            fs::write(mdir.join(format!("{marker}.txt")), page)?;
        }
    }
    Ok(())
}

// The `.gph` serializer (`render_menu_index`) and the atomic-publish primitives
// (`flip_current`, `gc`) now live in `gopher-core`. cta keeps its own snapshot
// orchestration (`publish`/`write_tree` above) and calls those primitives,
// passing its `KEEP_SNAPSHOTS` const into `gopher_core::gc`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transit::parse_positions;

    const FIXTURE: &str = include_str!("../fixtures/positions.json");

    fn fixture_positions() -> Positions {
        let mut p = parse_positions(FIXTURE).unwrap();
        p.from_fixture = true;
        p
    }

    /// Unique temp dir for a test, removed on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> TmpDir {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p =
                std::env::temp_dir().join(format!("gopher-cta-{tag}-{}-{ts}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publish_writes_tree_and_flips_current() {
        let tmp = TmpDir::new("publish");
        let geo = project::geometry();
        let atlas = Atlas::build(geo);
        let map_base = render::MapBase::build(&geo);
        let narration = NarrationView::default();
        let pos = fixture_positions();

        let snap = publish(
            &tmp.0, &pos, &geo, &atlas, &map_base, &narration, "CTA 'L'", None, None,
        )
        .unwrap();

        // current is a symlink to a relative out-* target
        let link = tmp.0.join("current");
        let target = fs::read_link(&link).unwrap();
        assert!(target.to_str().unwrap().starts_with("out-"));
        assert_eq!(tmp.0.join(&target), snap);

        // resolving current/ yields a complete tree
        let map = fs::read_to_string(link.join("map.txt")).unwrap();
        assert!(map.contains("CTA 'L'"));
        assert!(map.contains("trains plotted"));
        assert!(fs::read_to_string(link.join("about.txt"))
            .unwrap()
            .contains("gopher-cta"));
        // help pages are published
        assert!(fs::read_to_string(link.join("faq.txt"))
            .unwrap()
            .contains("Why braille?"));
        assert!(fs::read_to_string(link.join("help.txt"))
            .unwrap()
            .contains("Troubleshooting"));
        // the easter egg is in the tree but linked from no menu (hidden)
        assert!(fs::read_to_string(link.join("dig.txt"))
            .unwrap()
            .contains("You found the burrow"));
        assert!(!fs::read_to_string(link.join("index.gph"))
            .unwrap()
            .contains("/dig.txt"));

        // the atlas page is published alongside the braille map
        let atlas_txt = fs::read_to_string(link.join("atlas.txt")).unwrap();
        assert!(atlas_txt.contains("geographic atlas"));
        assert!(atlas_txt.contains("places named"));

        // ANSI colour variants published, and they actually carry SGR codes
        assert!(fs::read_to_string(link.join("map.ansi"))
            .unwrap()
            .contains("\x1b[38;5;"));
        assert!(fs::read_to_string(link.join("atlas.ansi"))
            .unwrap()
            .contains("\x1b[38;5;"));

        // the narrative panels are published (placeholders without a live Worker)
        assert!(fs::read_to_string(link.join("dispatch.txt"))
            .unwrap()
            .contains("feed stats:"));
        assert!(fs::read_to_string(link.join("sitrep.txt"))
            .unwrap()
            .contains("SITREP"));
        assert!(fs::read_to_string(link.join("events.txt"))
            .unwrap()
            .contains("event advisory"));
    }

    // (gc + .gph-escape tests moved to gopher-core, which owns those primitives.)

    #[test]
    fn menu_index_renders_geomyidae_gph() {
        let pos = fixture_positions();

        // Root: info banner stays a plain (info) line; map is a type-0 link; each
        // line is a type-1 submenu link. server/port are placeholder tokens.
        let root = gopher_core::render_menu_index(&render::root_menu(
            &pos,
            true,
            Some(("gopher.debene.dev", 7071)),
        ));
        assert!(root.contains("  gopher-cta : live CTA 'L' trains over Gopher\n"));
        assert!(root.contains("[0|Live train map (braille)|/map.txt|server|port]\n"));
        assert!(root.contains("[0|Geographic atlas (coast + landmarks)|/atlas.txt|server|port]\n"));
        assert!(root.contains("[1|Chicago landmarks|/landmarks|server|port]\n"));
        assert!(root.contains("[0|Dispatch (summary + feed stats)|/dispatch.txt|server|port]\n"));
        assert!(root.contains("[0|SITREP (AI alerts summary)|/sitrep.txt|server|port]\n"));
        assert!(root.contains("[0|Event advisory (AI)|/events.txt|server|port]\n"));
        assert!(root.contains("[1|Red      (5 running)|/red|server|port]\n"));
        assert!(root.contains("[0|FAQ|/faq.txt|server|port]\n"));
        assert!(root.contains("[0|Troubleshooting|/help.txt|server|port]\n"));
        // source tarball advertised as gopher type 9 (binary) when available
        assert!(
            root.contains("[9|Source code (tar.gz, fetch over gopher)|/src.tar.gz|server|port]\n")
        );
        // external links render as gopher type 'h' with a URL: selector
        assert!(root.contains(
            "[h|Source code (GitHub)|URL:https://github.com/felipedbene/gopher-cta|server|port]\n"
        ));
        assert!(root.contains("|URL:https://tracker.debene.dev/|server|port]\n"));
        // the phlog hub link is the ONE link that carries a concrete host/port
        // (a cross-server menu link); everything else stays placeholder tokens.
        assert!(root.contains("[1|Phlog -- the blog|/|gopher.debene.dev|7071]\n"));
        // never bake a real host/port into a *local* (this-tree) link
        assert!(!root.contains("localhost"));
        assert!(!root.contains("\t"));

        // Per-line: each train is a type-0 link to its detail page.
        let red = gopher_core::render_menu_index(&render::line_menu(&pos, "red"));
        assert!(red.contains("[0|Run 801   -> Howard"));
        assert!(red.contains("|/train/801.txt|server|port]\n"));
        assert!(red.contains("Red Line -- live trains\n")); // info header
    }

    #[test]
    fn phlog_link_adds_exactly_one_line_and_changes_nothing_else() {
        // Golden guard: the phlog hub link must be purely additive. Rendering the
        // root with vs without it may differ by exactly the one new line; every
        // existing link/info line stays byte-identical (no host/port leakage).
        let pos = fixture_positions();
        let without = gopher_core::render_menu_index(&render::root_menu(&pos, true, None));
        let with = gopher_core::render_menu_index(&render::root_menu(
            &pos,
            true,
            Some(("gopher.debene.dev", 7071)),
        ));
        let a: Vec<&str> = without.lines().collect();
        let b: Vec<&str> = with.lines().collect();
        assert_eq!(b.len(), a.len() + 1, "phlog must add exactly one line");

        let extra = "[1|Phlog -- the blog|/|gopher.debene.dev|7071]";
        assert!(b.contains(&extra), "the new line must be the phlog link");
        let rest: Vec<&str> = b.into_iter().filter(|l| *l != extra).collect();
        assert_eq!(rest, a, "all pre-existing lines must be byte-identical");
    }

    #[test]
    fn phlog_link_parsing() {
        assert_eq!(
            parse_phlog_link("gopher://gopher.debene.dev:7071").unwrap(),
            Some(("gopher.debene.dev".to_string(), 7071))
        );
        assert_eq!(
            parse_phlog_link("gopher://example.org").unwrap(),
            Some(("example.org".to_string(), 70))
        );
        assert_eq!(parse_phlog_link("none").unwrap(), None);
        assert_eq!(parse_phlog_link("").unwrap(), None);
        assert!(parse_phlog_link("http://x").is_err());
    }

    #[test]
    fn write_tree_builds_full_navigable_tree() {
        let tmp = TmpDir::new("tree");
        let pos = fixture_positions();
        let geo = project::geometry();
        let atlas = Atlas::build(geo);
        let map_base = render::MapBase::build(&geo);
        let narration = NarrationView::default();
        let snap = publish(
            &tmp.0, &pos, &geo, &atlas, &map_base, &narration, "CTA 'L'", None, None,
        )
        .unwrap();

        // root index links to the map, the atlas, and the red submenu
        let root = fs::read_to_string(snap.join("index.gph")).unwrap();
        assert!(root.contains("[0|Live train map (braille)|/map.txt|server|port]"));
        assert!(root.contains("[0|Geographic atlas (coast + landmarks)|/atlas.txt|server|port]"));
        // the atlas page itself is written
        assert!(fs::read_to_string(snap.join("atlas.txt"))
            .unwrap()
            .contains("geographic atlas"));
        assert!(root.contains("[1|Red      (5 running)|/red|server|port]"));

        // per-line submenu exists and drills into a train
        let red = fs::read_to_string(snap.join("red/index.gph")).unwrap();
        assert!(red.contains("[0|Run 801   -> Howard"));
        assert!(red.contains("|/train/801.txt|server|port]"));

        // the linked train detail page actually exists with matching content
        let train = fs::read_to_string(snap.join("train/801.txt")).unwrap();
        assert!(train.starts_with("Run 801 -- Red Line"));
        assert!(train.contains("destination: Howard"));

        // a detail page per running train (18 in the fixture)
        let n = fs::read_dir(snap.join("train")).unwrap().count();
        assert_eq!(n, pos.trains.len());

        // caps.txt is published verbatim with CRLF and the literal CAPS header.
        let caps = fs::read(snap.join("caps.txt")).unwrap();
        assert!(
            caps.starts_with(b"CAPS\r\n"),
            "caps.txt must start CAPS+CRLF"
        );
        assert!(
            caps.windows(2).any(|w| w == b"\r\n"),
            "caps.txt must be CRLF"
        );
        let caps_s = String::from_utf8_lossy(&caps);
        assert!(caps_s.contains("CapsVersion=1"));
        assert!(caps_s.contains("ServerSoftware=geomyidae"));
        // canonical (mis)spellings preserved
        assert!(caps_s.contains("PathDelimeter=/"));
        assert!(caps_s.contains("PathKeepPreDelimeter=FALSE"));

        // robots.txt is published and disallows the ephemeral train pages.
        let robots = fs::read_to_string(snap.join("robots.txt")).unwrap();
        assert!(robots.contains("User-agent: *"));
        assert!(robots.contains("Disallow: /train/"));

        // landmarks: root links the menu; the menu drills into a detail page;
        // the page exists with matching content.
        assert!(root.contains("[1|Chicago landmarks|/landmarks|server|port]"));
        let lm = fs::read_to_string(snap.join("landmarks/index.gph")).unwrap();
        assert!(lm.contains("/landmark/W.txt"));
        let willis = fs::read_to_string(snap.join("landmark/W.txt")).unwrap();
        assert!(willis.starts_with("Willis Tower"));
        assert!(willis.contains("category:     skyline"));
        // one detail page per landmark (14 in the overlay)
        let n = fs::read_dir(snap.join("landmark")).unwrap().count();
        assert_eq!(n, 14);
    }
}
