//! Skeletons taken from a store that already has them, instead of from the files.
//!
//!   phindex import-skeletons --db INDEX.sqlite --from SKELETONS.sqlite3
//!
//! A DNGC archive keeps a skeleton of every reference content, by SHA-256, in
//! `ARCHIVE/skeletons/skeletons.sqlite3` (dng-compare's archive format, section
//! 13). phidentify cuts files by the same rules and packs them with the same
//! compressor, so those rows are the ones this indexer would write. For each
//! row of the index that is at the revision before block marks and whose
//! SHA-256 the store has, this copies the skeleton, computes the block marks
//! from its block list, and stamps the row as current. No file of the tree is
//! opened: a row states the content it was hashed from, and the skeleton is of
//! that content.
//!
//! Rows whose content the store does not have are left as they are; the next
//! ordinary run upgrades them from the files. The store is opened read-only
//! and immutable. Another index's `skeletons` table serves as a store too.
use crate::{PREVIOUS_REVISION, REVISION, migrate};
use anyhow::{Context, Result, bail};
use phidentify::skeleton::{self, Skeleton};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

fn get(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

pub fn run(args: &[String]) -> Result<()> {
    let db_path = get(args, "--db").context("--db required")?;
    let from = get(args, "--from").context("--from required: a skeleton store (ARCHIVE/skeletons/skeletons.sqlite3)")?;
    if !Path::new(&db_path).is_file() {
        bail!("no index at {db_path}");
    }
    if !Path::new(&from).is_file() {
        bail!("no skeleton store at {from}");
    }
    let real = std::fs::canonicalize(&from)?.to_str().context("non-UTF-8 --from")?
        .replace('%', "%25").replace('?', "%3f").replace('#', "%23");
    let store = Connection::open_with_flags(format!("file:{real}?immutable=1"), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)?;
    let has: bool = store.query_row("SELECT count(*) = 3 FROM pragma_table_info('skeletons') WHERE name IN ('sha256','record','skeleton')",
                                    [], |r| r.get(0))?;
    if !has {
        bail!("{from} has no skeletons table");
    }
    let mut db = Connection::open(&db_path)?;
    db.execute_batch("PRAGMA journal_mode=WAL;")?;
    migrate(&db)?;
    let rows: Vec<(String, String)> = db.prepare("SELECT path, sha256 FROM files WHERE sha256 IS NOT NULL AND ids_rev IS ?1 ORDER BY path")?
        .query_map([PREVIOUS_REVISION], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    eprintln!("{} rows at the revision before block marks; skeletons from {from}", rows.len());
    let mut find = store.prepare("SELECT record, skeleton FROM skeletons WHERE sha256=?1")?;
    // Per content: its block marks or the reason it has none; None when the store lacks it.
    let mut seen: HashMap<String, Option<Result<String, String>>> = HashMap::new();
    let (mut upgraded, mut absent, mut unsound, mut copied, mut marked) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (t0, mut last) = (Instant::now(), Instant::now());
    let total = rows.len();
    for (n, chunk) in rows.chunks(2000).enumerate() {
        let tx = db.transaction()?;
        {
            let mut stamp = tx.prepare_cached("UPDATE files SET block_marks=?1, block_marks_err=?2, ids_rev=?3 WHERE path=?4")?;
            let mut keep = tx.prepare_cached("INSERT OR IGNORE INTO skeletons VALUES (?1,?2,?3)")?;
            for (path, sha) in chunk {
                if !seen.contains_key(sha) {
                    let found: Option<(String, Vec<u8>)> = find.query_row([sha], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    let marks = match found {
                        None => None,
                        // Checked as a reader checks it: the bytes against the record's own SHA-256, blocks in order.
                        Some((record, packed)) => match Skeleton::from_record(&record, &packed) {
                            Ok(s) => {
                                copied += keep.execute(params![sha, s.record(sha), packed])? as u64;
                                Some(skeleton::block_marks(&s))
                            }
                            Err(e) => {
                                unsound += 1;
                                if unsound <= 20 {
                                    eprintln!("not imported, {sha}: {e}");
                                }
                                None
                            }
                        },
                    };
                    seen.insert(sha.clone(), marks);
                }
                match &seen[sha] {
                    None => absent += 1,
                    Some(marks) => {
                        let (k, ke) = match marks {
                            Ok(m) => (Some(m.as_str()), None),
                            Err(e) => (None, Some(e.as_str())),
                        };
                        marked += k.is_some() as u64;
                        stamp.execute(params![k, ke, REVISION, path])?;
                        upgraded += 1;
                    }
                }
            }
        }
        tx.commit()?;
        if last.elapsed().as_secs() >= 30 {
            last = Instant::now();
            let done = ((n + 1) * 2000).min(total);
            let secs = t0.elapsed().as_secs_f64().max(1e-9);
            let eta = (total - done) as f64 * secs / done.max(1) as f64;
            eprintln!("{done}/{total} rows, {upgraded} upgraded, {absent} not in the store, ETA {}h{:02}m",
                      eta as u64 / 3600, eta as u64 % 3600 / 60);
        }
    }
    eprintln!("done: {upgraded} rows upgraded without opening a file ({marked} with block marks), {copied} skeletons copied; \
               {absent} rows left for the next run (their content is not in the store), {unsound} skeletons refused");
    Ok(())
}
