//! `phindex adopt`: rows for files that are copies of another tree's files,
//! taken from that tree's index instead of from the files.
//!
//!   phindex adopt --db INDEX.sqlite --root DIR --from OTHER.sqlite --from-root DIR
//!
//! After a synchronizer has copied files from one indexed tree into another,
//! every copy is new to the receiving tree's index, and an indexing run would
//! read each of them in full. The other index already has everything such a
//! read would find. For each file the other index lists under `--from-root`,
//! `adopt` looks at the same relative path under `--root`. If this index has
//! no current row for it, and the file there has the size and modification
//! time the other index has for its own, the row is written from the other
//! index: SHA-256, every identity, the metadata record and the skeleton, with
//! this file's own inode and device. No file of either tree is opened.
//!
//! The evidence that a file is a copy is its path, size and modification
//! time, so the copy has to have been made with times kept. A volume that
//! keeps whole seconds only is allowed for: a time that equals the other one
//! cut to the second is taken as equal. A file that differs in size or time
//! is left for the next ordinary run, which reads it. A path whose earlier
//! content this index knew gets that content listed in `versions`, as an
//! ordinary run would list it.
//!
//! The metadata record is the other file's, with the folder it names changed
//! to this tree's; its file-system dates remain those the other index saw.
use crate::conform::{get, open_index, open_ro};
use crate::{IDENTITY_COLUMNS, REVISION, deflate, lstat, now};
use anyhow::{Context, Result, bail};
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params, params_from_iter};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

const USAGE: &str = "usage: phindex adopt --db INDEX.sqlite --root DIR --from OTHER.sqlite --from-root DIR";

/// The other file's record with its folder under FROM_ROOT renamed to ROOT.
fn rerooted(record_deflate: &[u8], from_root: &str, root: &str) -> Result<Vec<u8>> {
    let mut text = String::new();
    flate2::read::DeflateDecoder::new(record_deflate).read_to_string(&mut text)?;
    let mut record: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(dir) = record.get_mut("File:System:Directory")
        && let Some(d) = dir.as_str().and_then(|d| d.strip_prefix(from_root)).filter(|d| d.is_empty() || d.starts_with('/'))
    {
        *dir = serde_json::Value::String(format!("{root}{d}"));
    }
    deflate(&serde_json::to_string(&record)?)
}

pub fn run(args: &[String]) -> Result<()> {
    let need = |flag: &str| get(args, flag).with_context(|| format!("{flag} required\n{USAGE}"));
    let (db_path, from) = (need("--db")?, need("--from")?);
    let (root, from_root) = (need("--root")?.trim_end_matches('/').to_string(), need("--from-root")?.trim_end_matches('/').to_string());
    if !Path::new(&root).is_dir() {
        bail!("{root} is not a folder here");
    }
    let other = open_ro(&from)?;
    let mut db = open_index(&db_path)?;

    // What this index already has: path -> (size, mtime_ns, read at the current revision).
    let mut mine: HashMap<String, (i64, i64, bool)> = HashMap::new();
    {
        let mut st = db.prepare("SELECT path, size, mtime_ns, sha256 IS NOT NULL AND ids_rev IS ?3 FROM files WHERE gone=0 AND path>=?1 AND path<?2")?;
        let mut q = st.query(params![format!("{root}/"), format!("{root}0"), REVISION])?;
        while let Some(r) = q.next()? {
            mine.insert(r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?));
        }
    }
    // A time equal to the other, or to the other cut to whole seconds.
    let same_time = |here: i64, there: i64| here == there || (here % 1_000_000_000 == 0 && here == there - there.rem_euclid(1_000_000_000));

    let ids = IDENTITY_COLUMNS.join(", ");
    let theirs: Vec<(String, i64, i64, bool)> = other.prepare("SELECT path, size, mtime_ns, sha256 IS NOT NULL AND ids_rev IS ?3 FROM files
                                                               WHERE gone=0 AND path>=?1 AND path<?2 ORDER BY path")?
        .query_map(params![format!("{from_root}/"), format!("{from_root}0"), REVISION], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if theirs.is_empty() {
        bail!("{from} lists no files under {from_root}");
    }
    eprintln!("adopt: {} files under {from_root} in {from}; {} rows under {root}", theirs.len(), mine.len());
    let mut row = other.prepare(&format!("SELECT sha256, hashed_unix, {ids} FROM files WHERE path=?1"))?;
    let mut record = other.prepare("SELECT engine, record_deflate FROM metadata WHERE path=?1")?;
    let mut skeleton = other.prepare("SELECT record, skeleton FROM skeletons WHERE sha256=?1")?;
    let slots: Vec<String> = (1..=9 + IDENTITY_COLUMNS.len()).map(|n| format!("?{n}")).collect();
    let insert = format!(
        "INSERT INTO files (path, size, mtime_ns, ino, dev, seen_unix, gone, sha256, hashed_unix, {ids}) VALUES ({})
         ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime_ns=excluded.mtime_ns, ino=excluded.ino, dev=excluded.dev,
           seen_unix=excluded.seen_unix, gone=0, sha256=excluded.sha256, hashed_unix=excluded.hashed_unix, {}",
        slots.join(","), IDENTITY_COLUMNS.iter().map(|c| format!("{c}=excluded.{c}")).collect::<Vec<_>>().join(", "));

    let (mut adopted, mut bytes, mut current, mut absent, mut differ, mut unread, mut skeletons) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let (t0, mut last) = (Instant::now(), Instant::now());
    let seen = now();
    let total = theirs.len();
    for (n, chunk) in theirs.chunks(2000).enumerate() {
        let tx = db.transaction()?;
        {
            let mut put = tx.prepare_cached(&insert)?;
            let mut version = tx.prepare_cached("INSERT OR IGNORE INTO versions (path, sha256, size, mtime_ns, replaced_unix)
                                                 SELECT path, sha256, size, mtime_ns, ?2 FROM files WHERE path=?1 AND sha256 IS NOT NULL AND sha256 <> ?3")?;
            let mut meta = tx.prepare_cached("INSERT OR REPLACE INTO metadata VALUES (?1,?2,?3)")?;
            let mut no_meta = tx.prepare_cached("DELETE FROM metadata WHERE path=?1")?;
            let mut keep = tx.prepare_cached("INSERT OR IGNORE INTO skeletons VALUES (?1,?2,?3)")?;
            for (path, size, mtime, read) in chunk {
                let here = format!("{root}{}", &path[from_root.len()..]);
                if matches!(mine.get(&here), Some((s, m, true)) if s == size && same_time(*m, *mtime)) {
                    current += 1;
                    continue;
                }
                let Some((false, id, dev)) = lstat(Path::new(&here)) else {
                    absent += 1;
                    continue;
                };
                if id.size != *size || !same_time(id.mtime_ns, *mtime) {
                    differ += 1;
                    continue;
                }
                if !read {
                    unread += 1;
                    continue;
                }
                let values: Vec<Value> = row.query_row([path], |r| (0..2 + IDENTITY_COLUMNS.len()).map(|i| r.get::<_, Value>(i)).collect())?;
                let Value::Text(sha) = &values[0] else { bail!("{path}: a read row without a SHA-256") };
                version.execute(params![here, seen, sha])?;
                let mut all: Vec<Value> = vec![Value::Text(here.clone()), Value::Integer(id.size), Value::Integer(id.mtime_ns), Value::Integer(id.ino),
                                               Value::Integer(dev), Value::Integer(seen), Value::Integer(0)];
                all.extend(values.iter().cloned());
                put.execute(params_from_iter(all))?;
                match record.query_row([path], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))).optional()? {
                    Some((engine, packed)) => meta.execute(params![here, engine, rerooted(&packed, &from_root, &root)?])?,
                    None => no_meta.execute([&here])?,
                };
                if let Some((rec, packed)) = skeleton.query_row([sha], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))).optional()? {
                    skeletons += keep.execute(params![sha, rec, packed])? as u64;
                }
                adopted += 1;
                bytes += id.size as u64;
            }
        }
        tx.commit()?;
        if last.elapsed().as_secs() >= 30 {
            last = Instant::now();
            eprintln!("+{}s {}/{total} looked at, {adopted} adopted, {current} already current", t0.elapsed().as_secs(), ((n + 1) * 2000).min(total));
        }
    }
    eprintln!("done: {adopted} rows adopted without opening a file ({:.1} GB of files, {skeletons} skeletons copied); {current} already current; \
               {absent} not there; {differ} there with another size or time, left for an ordinary run; {unread} not read in the other index",
              bytes as f64 / 1e9);
    Ok(())
}
