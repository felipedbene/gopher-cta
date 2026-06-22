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
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::atlas::Atlas;
use crate::project::{self, Geometry};
use crate::render;
use crate::transit::{Positions, TransitSource};

/// Published snapshots to retain (besides whatever `current` resolves to).
const KEEP_SNAPSHOTS: usize = 3;

/// Fetcher configuration, parsed from CLI args / env.
pub struct Config {
    pub out: PathBuf,
    pub once: bool,
    pub interval: Duration,
}

impl Config {
    /// Parse args after the `fetch` subcommand. `--once`, `--interval <secs>`
    /// (default 30), `--out <dir>` (default `$GOPHER_OUT` or `public`).
    pub fn from_args(args: &[String]) -> Result<Config, String> {
        let mut out =
            PathBuf::from(std::env::var("GOPHER_OUT").unwrap_or_else(|_| "public".into()));
        let mut once = false;
        let mut interval = Duration::from_secs(30);

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
                other => return Err(format!("unknown fetch arg: {other}")),
            }
        }
        Ok(Config {
            out,
            once,
            interval,
        })
    }
}

/// Run the fetcher: fetch -> publish, once or on an interval.
pub async fn run<S: TransitSource>(cfg: Config, source: S) -> io::Result<()> {
    let geo = project::geometry();
    // Rasterize the static geo overlay once; each publish clones it (never
    // re-rasterizes) and paints live trains on top.
    let atlas = Atlas::build(geo);
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
            Ok(pos) => match publish(&cfg.out, &pos, &geo, &atlas, source.name()) {
                Ok(snap) => eprintln!(
                    "[fetch] published {} ({} trains) -> {}/current",
                    snap.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    pos.trains.len(),
                    cfg.out.display()
                ),
                Err(e) => eprintln!("[fetch] publish failed: {e}"),
            },
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
fn publish(
    out: &Path,
    pos: &Positions,
    geo: &Geometry,
    atlas: &Atlas,
    source_name: &str,
) -> io::Result<PathBuf> {
    fs::create_dir_all(out)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| io::Error::other(e.to_string()))?
        .as_nanos();
    let snap = out.join(format!("out-{ts}"));
    fs::create_dir_all(&snap)?;
    write_tree(&snap, pos, geo, atlas, source_name)?;
    flip_current(out, &snap)?;
    gc(out, &snap)?;
    Ok(snap)
}

/// Write the full navigable gopher tree for one snapshot:
///   index.gph            root menu (map link + one entry per line)
///   map.txt              braille map (text)
///   atlas.txt            char-cell geographic atlas: coast + landmarks + trains
///   about.txt            about page (text)
///   <line>/index.gph     per-line menu, each train a drill-down link
///   train/<run>.txt      per-train detail page (text)
fn write_tree(
    dir: &Path,
    pos: &Positions,
    geo: &Geometry,
    atlas: &Atlas,
    source_name: &str,
) -> io::Result<()> {
    // Root menu + top-level text pages.
    fs::write(
        dir.join("index.gph"),
        render_menu_index(&render::root_menu(pos)),
    )?;
    fs::write(dir.join("map.txt"), render::map_page(pos, geo, source_name))?;
    fs::write(dir.join("atlas.txt"), atlas.render(pos, source_name))?;
    fs::write(dir.join("about.txt"), render::about_page())?;

    // One submenu directory per line (its index.gph is the per-line listing).
    for &line in render::LINE_ORDER {
        let ldir = dir.join(line);
        fs::create_dir_all(&ldir)?;
        fs::write(
            ldir.join("index.gph"),
            render_menu_index(&render::line_menu(pos, line)),
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
    Ok(())
}

/// The single daemon-specific function: serialize a daemon-agnostic menu
/// ([`render::Entry`] list) into a geomyidae `.gph` index. **To target a
/// different daemon (e.g. Gophernicus `gophermap`), rewrite only this.**
///
/// Format (confirmed against geomyidae(8) and the phd implementation): a link is
/// `[<type>|<name>|<selector>|server|port]`; geomyidae substitutes the literal
/// tokens `server`/`port` with its own host/port at serve time, so the files
/// stay host/port-agnostic. Any line not starting with `[` is an info (i) line.
fn render_menu_index(entries: &[render::Entry]) -> String {
    use render::{Entry, ItemKind};
    let mut out = String::new();
    for e in entries {
        match e {
            Entry::Info(s) => {
                // Info text that happens to start with '[' would be mis-parsed as
                // a link; a leading space keeps it an info line.
                if s.starts_with('[') {
                    out.push(' ');
                }
                out.push_str(s);
                out.push('\n');
            }
            Entry::Link {
                kind,
                display,
                selector,
            } => {
                let t = match kind {
                    ItemKind::Text => '0',
                    ItemKind::Menu => '1',
                };
                out.push_str(&format!(
                    "[{t}|{}|{}|server|port]\n",
                    gph_escape(display),
                    gph_escape(selector),
                ));
            }
        }
    }
    out
}

/// Escape the `.gph` field separator `|` within a field (geomyidae uses `\|`).
fn gph_escape(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Atomically point `current` at `snap`: write a temp symlink then rename it over
/// `current`. rename(2) is atomic, so a reader resolves either the old target or
/// the new one — never a missing/half-built link. The link is relative
/// (`current -> out-<ts>`) so it stays valid under any mount path.
fn flip_current(out: &Path, snap: &Path) -> io::Result<()> {
    let target = snap.file_name().expect("snapshot dir has a file name");
    let tmp = out.join(format!(".current.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    symlink(target, &tmp)?;
    fs::rename(&tmp, out.join("current"))
}

/// Remove old `out-*` snapshots, keeping the newest [`KEEP_SNAPSHOTS`] and never
/// the one just published.
fn gc(out: &Path, keep: &Path) -> io::Result<()> {
    let mut snaps: Vec<PathBuf> = fs::read_dir(out)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("out-"))
        })
        .collect();
    snaps.sort(); // nanosecond names sort chronologically; newest last
    let n = snaps.len();
    let keep_name = keep.file_name();
    for (i, p) in snaps.iter().enumerate() {
        let is_recent = i + KEEP_SNAPSHOTS >= n;
        let is_current = p.file_name() == keep_name;
        if !is_recent && !is_current {
            let _ = fs::remove_dir_all(p);
        }
    }
    Ok(())
}

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
        let pos = fixture_positions();

        let snap = publish(&tmp.0, &pos, &geo, &atlas, "CTA 'L'").unwrap();

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

        // the atlas page is published alongside the braille map
        let atlas_txt = fs::read_to_string(link.join("atlas.txt")).unwrap();
        assert!(atlas_txt.contains("geographic atlas"));
        assert!(atlas_txt.contains("LANDMARKS"));
    }

    #[test]
    fn gc_keeps_recent_plus_current_and_drops_the_rest() {
        let tmp = TmpDir::new("gc");
        // Six chronological snapshots: out-000 .. out-005
        let mut dirs = Vec::new();
        for i in 0..6 {
            let d = tmp.0.join(format!("out-{i:03}"));
            fs::create_dir_all(&d).unwrap();
            dirs.push(d);
        }
        // Pretend out-000 is the current target (oldest) — must be retained even
        // though it's not among the newest KEEP_SNAPSHOTS.
        gc(&tmp.0, &dirs[0]).unwrap();

        let remaining: std::collections::BTreeSet<String> = fs::read_dir(&tmp.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("out-"))
            .collect();
        // newest 3 (003,004,005) + the protected current (000) = 4
        assert_eq!(
            remaining.len(),
            KEEP_SNAPSHOTS + 1,
            "remaining: {remaining:?}"
        );
        assert!(remaining.contains("out-000")); // current protected
        assert!(remaining.contains("out-005")); // newest
        assert!(!remaining.contains("out-001")); // dropped
        assert!(!remaining.contains("out-002")); // dropped
    }

    #[test]
    fn menu_index_renders_geomyidae_gph() {
        let pos = fixture_positions();

        // Root: info banner stays a plain (info) line; map is a type-0 link; each
        // line is a type-1 submenu link. server/port are placeholder tokens.
        let root = render_menu_index(&render::root_menu(&pos));
        assert!(root.contains("  gopher-cta : live CTA 'L' trains over Gopher\n"));
        assert!(root.contains("[0|Live train map (braille)|/map.txt|server|port]\n"));
        assert!(root.contains("[0|Geographic atlas (coast + landmarks)|/atlas.txt|server|port]\n"));
        assert!(root.contains("[1|Red      (5 running)|/red|server|port]\n"));
        // never bake a real host/port into the static index
        assert!(!root.contains("localhost"));
        assert!(!root.contains("\t"));

        // Per-line: each train is a type-0 link to its detail page.
        let red = render_menu_index(&render::line_menu(&pos, "red"));
        assert!(red.contains("[0|Run 801   -> Howard|/train/801.txt|server|port]\n"));
        assert!(red.contains("Red Line -- live trains\n")); // info header
    }

    #[test]
    fn gph_escape_escapes_pipe() {
        assert_eq!(gph_escape("a|b"), "a\\|b");
        assert_eq!(gph_escape("plain"), "plain");
    }

    #[test]
    fn write_tree_builds_full_navigable_tree() {
        let tmp = TmpDir::new("tree");
        let pos = fixture_positions();
        let geo = project::geometry();
        let atlas = Atlas::build(geo);
        let snap = publish(&tmp.0, &pos, &geo, &atlas, "CTA 'L'").unwrap();

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
        assert!(red.contains("[0|Run 801   -> Howard|/train/801.txt|server|port]"));

        // the linked train detail page actually exists with matching content
        let train = fs::read_to_string(snap.join("train/801.txt")).unwrap();
        assert!(train.starts_with("Run 801 -- Red Line"));
        assert!(train.contains("destination: Howard"));

        // a detail page per running train (18 in the fixture)
        let n = fs::read_dir(snap.join("train")).unwrap().count();
        assert_eq!(n, pos.trains.len());
    }
}
