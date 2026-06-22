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
            Ok(pos) => match publish(&cfg.out, &pos, &geo, source.name()) {
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
fn publish(out: &Path, pos: &Positions, geo: &Geometry, source_name: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(out)?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| io::Error::other(e.to_string()))?
        .as_nanos();
    let snap = out.join(format!("out-{ts}"));
    fs::create_dir_all(&snap)?;
    write_tree(&snap, pos, geo, source_name)?;
    flip_current(out, &snap)?;
    gc(out, &snap)?;
    Ok(snap)
}

/// Write the static files for one snapshot.
///
/// COMMIT 2 writes the text pages; COMMIT 3 expands this to the full navigable
/// tree (root + per-line menus, per-train pages) with the daemon's index format.
fn write_tree(dir: &Path, pos: &Positions, geo: &Geometry, source_name: &str) -> io::Result<()> {
    fs::write(dir.join("map.txt"), render::map_page(pos, geo, source_name))?;
    fs::write(dir.join("about.txt"), render::about_page())?;
    Ok(())
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
        let pos = fixture_positions();

        let snap = publish(&tmp.0, &pos, &geo, "CTA 'L'").unwrap();

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
}
