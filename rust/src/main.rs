//! phindex: a resumable photo-identity index of a directory tree in SQLite. One
//! binary with N workers. Its OxCaml counterpart, `phindex-ox` (ocaml/), writes
//! the same database with the same revision stamp, so either continues the
//! other's index.
//!
//!   phindex --db INDEX.sqlite --root DIR [--only SUBDIR] [--full-stat] [--workers N] [--progress-secs S]
//!
//! Each file read records SHA-256, DNG raw blocks, Lightroom's fingerprint
//! (supported DNGs), the metadata digest and compacted record, and the tiffgraph
//! identity: dngsem_profile (strictest of dng-xmp/0, /1, /2 accepting the DNG)
//! and preserved (Semantic Unison's SDE/0-preserved digest of every byte outside
//! its XMP packet). All are taken from one read of the file. A failed or
//! inapplicable identity leaves its value NULL and its reason in the _err column.
//! Files are only read. New, changed and unfinished files are read; vanished
//! ones are marked gone. One writer commits every 500 results; workers stream
//! results to it, so progress prints on time and an interrupted run resumes.
//!
//! Quick walk (default): directories whose inode and mtime are unchanged are not
//! listed and their files not stat'ed; they come from the index. --full-stat
//! stats every file (catches a file rewritten in place under the same name).
//! A new path whose device, inode, size and mtime equal a vanished, fully
//! identified row inherits its identities (moves and renames) without a read.

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The identity definitions rows are stamped with, identical in phindex-ox. A row
/// with another stamp is read again, so change it only when an identity's
/// definition changes, never for a release that leaves them alone.
const REVISION: &str = "phidentify 0.1.0 +lightroom +metadata; tiffgraph dng-xmp/0,1,2 max_xmp=1MiB; SDE/0-preserved";
const ENGINE: &str = "exiftool-rs (via phidentify)";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
  path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL, ino INTEGER NOT NULL,
  dev INTEGER NOT NULL, sha256 TEXT, hashed_unix INTEGER, seen_unix INTEGER NOT NULL, gone INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS files_sha ON files(sha256);
CREATE INDEX IF NOT EXISTS files_size ON files(size);
CREATE TABLE IF NOT EXISTS runs (root TEXT, started_unix INTEGER, finished_unix INTEGER, walked INTEGER,
  hashed INTEGER, bytes_hashed INTEGER, gone INTEGER, errors INTEGER);
CREATE TABLE IF NOT EXISTS metadata (path TEXT PRIMARY KEY, engine TEXT NOT NULL, record_deflate BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS dirs (path TEXT PRIMARY KEY, ino INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
  seen_unix INTEGER NOT NULL, gone INTEGER NOT NULL DEFAULT 0);
";
const IDENTITY_COLUMNS: &[&str] = &["dng_blocks", "dng_blocks_err", "lr_digest", "lr_err", "md_digest", "md_err", "ids_rev",
                                    "dngsem_profile", "dngsem_err", "preserved", "preserved_err"];

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Ident {
    size: i64,
    mtime_ns: i64,
    ino: i64,
}

#[derive(Clone)]
struct Entry {
    path: String,
    ident: Ident,
    dev: i64,
}

struct Known {
    ident: Ident,
    done: bool,
    sha: Option<String>,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// (is_dir, ident, dev) for a directory or regular file; None for anything else.
fn lstat(p: &Path) -> Option<(bool, Ident, i64)> {
    let m = fs::symlink_metadata(p).ok()?;
    if !(m.is_dir() || m.is_file()) {
        return None;
    }
    Some((m.is_dir(), Ident { size: m.size() as i64, mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(), ino: m.ino() as i64 },
          m.dev() as i64))
}

fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA)?;
    let have: HashSet<String> = db.prepare("SELECT name FROM pragma_table_info('files')")?
        .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for c in IDENTITY_COLUMNS {
        if !have.contains(*c) {
            db.execute_batch(&format!("ALTER TABLE files ADD COLUMN {c} TEXT"))?;
        }
    }
    db.execute_batch("CREATE INDEX IF NOT EXISTS files_dng_blocks ON files(dng_blocks);
                      CREATE INDEX IF NOT EXISTS files_lr ON files(lr_digest);
                      CREATE INDEX IF NOT EXISTS files_md ON files(md_digest);
                      CREATE INDEX IF NOT EXISTS files_preserved ON files(preserved);")?;
    Ok(())
}

struct Walk {
    entries: Vec<Entry>,
    dirs: Vec<(String, i64, i64)>,
    listed: usize,
    reused: usize,
    errors: u64,
}

/// Every regular file under root, and every directory visited with its inode and
/// mtime (taken before listing it). Unchanged directories come from the index.
fn walk(root: &str, unchanged: &dyn Fn(&str, i64, i64) -> bool, files_of: &HashMap<String, Vec<Entry>>,
        subdirs_of: &HashMap<String, Vec<String>>) -> Result<Walk> {
    let mut w = Walk { entries: Vec::new(), dirs: Vec::new(), listed: 0, reused: 0, errors: 0 };
    let Some((true, rid, _)) = lstat(Path::new(root)) else { bail!("not a directory: {root}") };
    let mut stack = vec![(root.to_string(), rid)];
    while let Some((d, id)) = stack.pop() {
        w.dirs.push((d.clone(), id.ino, id.mtime_ns));
        if unchanged(&d, id.ino, id.mtime_ns) {
            w.reused += 1;
            if let Some(fs) = files_of.get(&d) {
                w.entries.extend(fs.iter().cloned());
            }
            for s in subdirs_of.get(&d).into_iter().flatten() {
                if let Some((true, sid, _)) = lstat(Path::new(s)) {
                    stack.push((s.clone(), sid));
                }
            }
            continue;
        }
        w.listed += 1;
        let Ok(rd) = fs::read_dir(&d) else {
            w.errors += 1;
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            match lstat(&p) {
                None => {}
                Some((is_dir, ident, dev)) => match p.to_str() {
                    None => w.errors += 1, // non-UTF-8 name: not indexable as TEXT
                    Some(s) if is_dir => stack.push((s.to_string(), ident)),
                    Some(s) => w.entries.push(Entry { path: s.to_string(), ident, dev }),
                },
            }
        }
    }
    Ok(w)
}

fn split(o: &phidentify::Outcome) -> (Option<&str>, Option<&str>) {
    match o {
        None => (None, None),
        Some(Ok(v)) => (Some(v.as_str()), None),
        Some(Err(e)) => (None, Some(e.as_str())),
    }
}

fn deflate(text: &str) -> Result<Vec<u8>> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(text.as_bytes())?;
    Ok(e.finish()?)
}

fn update(db_path: &str, root: &str, only: Option<&str>, full_stat: bool, workers: usize, progress: Duration) -> Result<()> {
    let rev = REVISION;
    let root = fs::canonicalize(root).with_context(|| format!("root {root}"))?.to_str().context("non-UTF-8 root")?.to_owned();
    let scope = match only {
        None => root.clone(),
        Some(d) => {
            let d = fs::canonicalize(d)?.to_str().context("non-UTF-8 --only")?.to_owned();
            if !d.starts_with(&format!("{root}/")) {
                bail!("--only must be under --root: {d}");
            }
            d
        }
    };
    let mut db = Connection::open(db_path)?;
    db.execute_batch("PRAGMA journal_mode=WAL;")?;
    migrate(&db)?;
    let started = now();
    let (lo, hi) = (format!("{scope}/"), format!("{scope}0"));

    let mut known: HashMap<String, Known> = HashMap::new();
    let mut was_gone: HashSet<String> = HashSet::new();
    let mut files_by_dir: HashMap<String, Vec<Entry>> = HashMap::new();
    {
        let mut st = db.prepare("SELECT path, size, mtime_ns, ino, sha256 IS NOT NULL AND ids_rev IS ?3, sha256, gone, dev
                                 FROM files WHERE path >= ?1 AND path < ?2")?;
        let mut rows = st.query(params![lo, hi, rev])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            let ident = Ident { size: r.get(1)?, mtime_ns: r.get(2)?, ino: r.get(3)? };
            if r.get::<_, bool>(6)? {
                was_gone.insert(path.clone());
            } else {
                let dir = Path::new(&path).parent().and_then(|p| p.to_str()).unwrap_or("").to_string();
                files_by_dir.entry(dir).or_default().push(Entry { path: path.clone(), ident, dev: r.get(7)? });
            }
            known.insert(path, Known { ident, done: r.get(4)?, sha: r.get(5)? });
        }
    }
    let mut dirs_known: HashMap<String, (i64, i64)> = HashMap::new();
    let mut subdirs: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut st = db.prepare("SELECT path, ino, mtime_ns FROM dirs WHERE gone=0 AND (path=?1 OR (path >= ?2 AND path < ?3))")?;
        let mut rows = st.query(params![scope, lo, hi])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            if path != scope {
                let parent = Path::new(&path).parent().and_then(|p| p.to_str()).unwrap_or("").to_string();
                subdirs.entry(parent).or_default().push(path.clone());
            }
            dirs_known.insert(path, (r.get(1)?, r.get(2)?));
        }
    }

    let t_walk = Instant::now();
    let unchanged = |d: &str, ino: i64, mtime: i64| !full_stat && dirs_known.get(d) == Some(&(ino, mtime));
    let w = walk(&scope, &unchanged, &files_by_dir, &subdirs)?;
    let mut errors = w.errors;
    eprintln!("walk: {} directories, {} listed, {} unchanged and taken from the index{}, {:.0} s", w.dirs.len(), w.listed, w.reused,
              if full_stat { " (--full-stat)" } else { "" }, t_walk.elapsed().as_secs_f64());
    let entries = w.entries;
    let seen = now();
    let present: HashSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    let changed: Vec<&Entry> = entries.iter()
        .filter(|e| known.get(&e.path).is_none_or(|k| k.ident != e.ident || !k.done)).collect();
    let fresh: Vec<&Entry> = changed.iter().copied().filter(|e| known.get(&e.path).is_none_or(|k| k.ident != e.ident)).collect();
    let gone: Vec<&String> = known.keys().filter(|p| !present.contains(p.as_str()) && !was_gone.contains(*p)).collect();
    // A path marked gone that is back, unchanged, is present again (a changed one is reset as fresh).
    let returned: Vec<&str> = entries.iter()
        .filter(|e| was_gone.contains(&e.path) && known.get(&e.path).is_some_and(|k| k.ident == e.ident))
        .map(|e| e.path.as_str()).collect();

    // Moves and renames: a new path with the device, inode, size and mtime of a fully
    // identified row that is gone inherits that row.
    // Every vanished row with the same identity is marked gone (a file moved twice
    // leaves two), including sources outside --only, which this walk didn't cover.
    let mut moved: HashMap<String, String> = HashMap::new();
    let mut moved_from: Vec<String> = Vec::new();
    {
        let mut st = db.prepare("SELECT path, gone FROM files WHERE dev=?1 AND ino=?2 AND size=?3 AND mtime_ns=?4 AND ids_rev IS ?5
                                 AND sha256 IS NOT NULL AND path <> ?6")?;
        for e in fresh.iter().filter(|e| !known.contains_key(&e.path)) {
            let rows: Vec<(String, bool)> = st.query_map(params![e.dev, e.ident.ino, e.ident.size, e.ident.mtime_ns, rev, e.path],
                                                          |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            // Only from paths that no longer exist: two live hard links are read separately.
            let vanished: Vec<(String, bool)> =
                rows.into_iter().filter(|(p, _)| !present.contains(p.as_str()) && !Path::new(p).exists()).collect();
            if let Some((old, _)) = vanished.first() {
                moved.insert(e.path.clone(), old.clone());
                moved_from.extend(vanished.iter().filter(|(_, g)| !g).map(|(p, _)| p.clone()));
            }
        }
    }
    {
        let tx = db.transaction()?;
        {
            let mut up = tx.prepare(
                "INSERT INTO files (path,size,mtime_ns,ino,dev,sha256,hashed_unix,seen_unix,gone) VALUES (?1,?2,?3,?4,?5,NULL,NULL,?6,0)
                 ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime_ns=excluded.mtime_ns, ino=excluded.ino, dev=excluded.dev,
                 sha256=NULL, hashed_unix=NULL, seen_unix=excluded.seen_unix, gone=0, dng_blocks=NULL, dng_blocks_err=NULL,
                 lr_digest=NULL, lr_err=NULL, md_digest=NULL, md_err=NULL, ids_rev=NULL, dngsem_profile=NULL, dngsem_err=NULL,
                 preserved=NULL, preserved_err=NULL")?;
            let mut m = tx.prepare("DELETE FROM metadata WHERE path=?1")?;
            for e in &fresh {
                up.execute(params![e.path, e.ident.size, e.ident.mtime_ns, e.ident.ino, e.dev, seen])?;
                m.execute(params![e.path])?;
            }
            let mut g = tx.prepare("UPDATE files SET gone=1 WHERE path=?1")?;
            for p in &gone {
                g.execute(params![p])?;
            }
            let mut back = tx.prepare("UPDATE files SET gone=0 WHERE path=?1")?;
            for p in &returned {
                back.execute(params![p])?;
            }
            let mut d = tx.prepare("INSERT INTO dirs (path, ino, mtime_ns, seen_unix, gone) VALUES (?1,?2,?3,?4,0)
                ON CONFLICT(path) DO UPDATE SET ino=excluded.ino, mtime_ns=excluded.mtime_ns, seen_unix=excluded.seen_unix, gone=0")?;
            let mut seen_dirs = HashSet::new();
            for (p, ino, mtime) in &w.dirs {
                seen_dirs.insert(p.as_str());
                d.execute(params![p, ino, mtime, seen])?;
            }
            let mut dg = tx.prepare("UPDATE dirs SET gone=1 WHERE path=?1")?;
            for p in dirs_known.keys().filter(|p| !seen_dirs.contains(p.as_str())) {
                dg.execute(params![p])?;
            }
            let mut mv = tx.prepare("UPDATE files SET (sha256, hashed_unix, dng_blocks, dng_blocks_err, lr_digest, lr_err, md_digest,
                md_err, ids_rev, dngsem_profile, dngsem_err, preserved, preserved_err) = (SELECT sha256, hashed_unix, dng_blocks,
                dng_blocks_err, lr_digest, lr_err, md_digest, md_err, ids_rev, dngsem_profile, dngsem_err, preserved, preserved_err
                FROM files WHERE path=?2) WHERE path=?1")?;
            let mut mm = tx.prepare("INSERT OR REPLACE INTO metadata SELECT ?1, engine, record_deflate FROM metadata WHERE path=?2")?;
            for (p, old) in &moved {
                mv.execute(params![p, old])?;
                mm.execute(params![p, old])?;
            }
            for old in &moved_from {
                g.execute(params![old])?;
            }
        }
        tx.commit()?;
    }

    let todo: Vec<&Entry> = changed.into_iter().filter(|e| !moved.contains_key(&e.path)).collect();
    let total: i64 = todo.iter().map(|e| e.ident.size).sum();
    eprintln!("walked {} files under {scope}, {} to read ({:.1} GB), {} moved or renamed (identities kept), {} gone, {errors} errors, \
               {workers} workers (phindex), identities: {rev}",
              entries.len(), todo.len(), total as f64 / 1e9, moved.len(), gone.len());
    let stored_sha: HashMap<String, String> = todo.iter()
        .filter_map(|e| known.get(&e.path).filter(|k| k.ident == e.ident).and_then(|k| k.sha.clone()).map(|s| (e.path.clone(), s)))
        .collect();

    let opts = phidentify::Options { dng_blocks: true, lightroom: true, metadata: true, preserved_regions: None, dng_semantic: true };
    let jobs: Vec<(String, Ident)> = todo.iter().map(|e| (e.path.clone(), e.ident)).collect();
    let n_todo = jobs.len();
    let (tx, rx) = mpsc::sync_channel::<(String, i64, Result<phidentify::Identity, String>)>(256);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(workers).build()?;
    let producer = std::thread::spawn(move || {
        pool.install(|| {
            jobs.into_par_iter().for_each_with(tx, |tx, (path, before)| {
                let r = phidentify::identify(Path::new(&path), opts.clone()).map_err(|e| format!("{e:#}")).and_then(|id| {
                    match lstat(Path::new(&path)) {
                        Some((false, after, _)) if after == before => Ok(id),
                        _ => Err("changed since the walk".to_string()),
                    }
                });
                let _ = tx.send((path, before.size, r));
            })
        })
    });

    let t0 = Instant::now();
    let mut last = Instant::now();
    let (mut n, mut hashed, mut bytes, mut sha_changed) = (0u64, 0u64, 0i64, 0u64);
    let (mut cb, mut cl, mut cm, mut cs) = (0u64, 0u64, 0u64, 0u64);
    let mut batch = db.transaction()?;
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok((path, size, result)) => {
                n += 1;
                match result {
                    Ok(id) => {
                        if stored_sha.get(&path).is_some_and(|old| *old != id.sha256) {
                            sha_changed += 1;
                            errors += 1;
                            eprintln!("SHA256 CHANGED with unchanged size/mtime/inode: {path}");
                        }
                        let (b, be) = split(&id.dng_raw_blocks_sha256);
                        let (l, le) = split(&id.lightroom_digest);
                        let (m, me) = split(&id.metadata_digest);
                        let (s, se) = split(&id.dng_semantic_profile);
                        let (p, pe) = split(&id.preserved_sha256);
                        cb += b.is_some() as u64;
                        cl += l.is_some() as u64;
                        cm += m.is_some() as u64;
                        cs += p.is_some() as u64;
                        batch.execute("UPDATE files SET sha256=?1, hashed_unix=?2, dng_blocks=?3, dng_blocks_err=?4, lr_digest=?5,
                                       lr_err=?6, md_digest=?7, md_err=?8, ids_rev=?9, dngsem_profile=?10, dngsem_err=?11,
                                       preserved=?12, preserved_err=?13 WHERE path=?14",
                                      params![id.sha256, now(), b, be, l, le, m, me, rev, s, se, p, pe, path])?;
                        if let Some(json) = &id.metadata_json {
                            let record = phidentify::metadata::compact(json)?;
                            batch.execute("INSERT OR REPLACE INTO metadata VALUES (?1,?2,?3)", params![path, ENGINE, deflate(&record)?])?;
                        }
                        hashed += 1;
                        bytes += size;
                    }
                    Err(e) => {
                        errors += 1;
                        if errors <= 50 {
                            eprintln!("ERROR {path}: {e}");
                        }
                    }
                }
                if n % 500 == 0 {
                    batch.commit()?;
                    batch = db.transaction()?;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last.elapsed() >= progress {
            last = Instant::now();
            let secs = t0.elapsed().as_secs_f64().max(1e-9);
            let rate = bytes as f64 / secs;
            let eta = if rate > 0.0 { (total - bytes).max(0) as f64 / rate } else { 0.0 };
            let el = t0.elapsed().as_secs();
            eprintln!("+{}h{:02}m {n}/{n_todo} files, {:.1}/{:.1} GB ({:.1}%), {:.0} MB/s, {errors} errors, ETA {:.0}h{:02.0}m; \
                       raw blocks {cb}, Lightroom {cl}, metadata {cm}, preserved {cs}",
                      el / 3600, el % 3600 / 60, bytes as f64 / 1e9, total as f64 / 1e9, 100.0 * bytes as f64 / total.max(1) as f64,
                      rate / 1e6, (eta / 3600.0).floor(), (eta % 3600.0) / 60.0);
        }
    }
    batch.commit()?;
    producer.join().map_err(|_| anyhow::anyhow!("worker pool panicked"))?;
    db.execute("INSERT INTO runs VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
               params![scope, started, now(), entries.len() as i64, hashed as i64, bytes, gone.len() as i64, errors as i64])?;
    eprintln!("done: read {hashed} files, {:.1} GB, {errors} errors ({sha_changed} stored SHA-256 mismatches); raw blocks {cb}, \
               Lightroom {cl}, metadata {cm}, preserved {cs}", bytes as f64 / 1e9);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let workers: usize = get("--workers").map(|w| w.parse()).transpose()?.unwrap_or(8);
    if workers == 0 {
        bail!("--workers must be at least 1");
    }
    let progress = Duration::from_secs(get("--progress-secs").map(|s| s.parse()).transpose()?.unwrap_or(60));
    update(&get("--db").context("--db required")?, &get("--root").context("--root required")?, get("--only").as_deref(),
           args.iter().any(|a| a == "--full-stat"), workers, progress)
}
