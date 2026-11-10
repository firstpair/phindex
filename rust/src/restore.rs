//! Earlier contents of indexed files, rebuilt from their skeletons.
//!
//!   phindex versions --db INDEX.sqlite [--path FILE]
//!   phindex restore --db INDEX.sqlite (--path FILE [--sha256 HEX] | --sha256 HEX) --out FILE [--from EDITED]
//!
//! Each run of the indexer keeps, for every content it reads, the content's
//! skeleton (`skeletons`, by SHA-256), and, when a path's content has changed
//! since the last run, the earlier content's SHA-256 (`versions`). A metadata
//! edit carries a file's large blocks over unchanged, so the earlier content is
//! the skeleton with those blocks taken from the file as it is now.
//!
//! `restore` writes one content to --out, which must not exist. The blocks come
//! from --from when given, otherwise from the first of these that rebuilds it:
//! the file at --path, a live file the index lists with that SHA-256, a live
//! file listed with the same block marks. What is written is checked against
//! the SHA-256; on a mismatch nothing is left behind. Only the index is read,
//! and it is not changed.
use anyhow::{Context, Result, bail};
use phidentify::skeleton::{self, Skeleton};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::fs;
use std::path::{Path, PathBuf};

/// The most files tried when none is named.
const CANDIDATES: usize = 8;

fn get(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

/// The index, read-only. An idle index is opened immutable, which writes
/// nothing beside it; one with a write-ahead log is read as it is.
fn open(db: &str) -> Result<Connection> {
    if !Path::new(db).is_file() {
        bail!("no index at {db}");
    }
    let wal = Path::new(&format!("{db}-wal")).exists();
    let c = if wal {
        Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
    } else {
        Connection::open_with_flags(format!("file:{db}?immutable=1"), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
    }?;
    let ready: bool = c.query_row("SELECT count(*) = 2 FROM sqlite_master WHERE type='table' AND name IN ('skeletons','versions')",
                                  [], |r| r.get(0))?;
    if !ready {
        bail!("this index has no skeletons yet: run phindex on its tree first");
    }
    Ok(c)
}

fn absolute(path: &str) -> String {
    fs::canonicalize(path).ok().and_then(|p| p.to_str().map(str::to_owned)).unwrap_or_else(|| path.to_owned())
}

pub fn versions(args: &[String]) -> Result<()> {
    let db = open(&get(args, "--db").context("--db required")?)?;
    let path = get(args, "--path").map(|p| absolute(&p));
    let mut st = db.prepare("SELECT v.path, v.sha256, v.size, v.mtime_ns, v.replaced_unix,
                             EXISTS (SELECT 1 FROM skeletons s WHERE s.sha256 = v.sha256)
                             FROM versions v WHERE ?1 IS NULL OR v.path = ?1 ORDER BY v.path, v.replaced_unix, v.mtime_ns")?;
    let mut rows = st.query(params![path])?;
    let mut n = 0;
    println!("path\tsha256\tsize\tmtime_ns\treplaced_unix\tskeleton");
    while let Some(r) = rows.next()? {
        n += 1;
        println!("{}\t{}\t{}\t{}\t{}\t{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?,
                 r.get::<_, i64>(4)?, if r.get::<_, bool>(5)? { "yes" } else { "no" });
    }
    eprintln!("{n} earlier contents");
    Ok(())
}

/// The skeleton of the content with this SHA-256.
fn load(db: &Connection, sha256: &str) -> Result<Skeleton> {
    let row: Option<(String, Vec<u8>)> = db.query_row("SELECT record, skeleton FROM skeletons WHERE sha256=?1", [sha256],
                                                      |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let (record, packed) = row.with_context(|| format!("the index holds no skeleton for {sha256}"))?;
    Skeleton::from_record(&record, &packed).map_err(anyhow::Error::msg)
}

/// Live files worth trying, in order, without repeats.
fn candidates(db: &Connection, sha256: &str, skeleton: &Skeleton, path: Option<&str>) -> Result<Vec<String>> {
    let mut out: Vec<String> = path.map(str::to_owned).into_iter().collect();
    let mut add = |sql: &str, value: &str| -> Result<()> {
        let mut st = db.prepare(sql)?;
        for p in st.query_map(params![value, (4 * CANDIDATES) as i64], |r| r.get::<_, String>(0))? {
            let p = p?;
            if !out.contains(&p) {
                out.push(p);
            }
        }
        Ok(())
    };
    add("SELECT path FROM files WHERE sha256=?1 AND gone=0 ORDER BY path LIMIT ?2", sha256)?;
    if let Ok(marks) = skeleton::block_marks(skeleton) {
        add("SELECT path FROM files WHERE block_marks=?1 AND gone=0 ORDER BY path LIMIT ?2", &marks)?;
    }
    Ok(out)
}

/// Write the content to `tmp` from `edited`; whether it is the content.
fn attempt(skeleton: &Skeleton, sha256: &str, edited: &Path, tmp: &Path) -> Result<bool, String> {
    let mut f = fs::File::create(tmp).map_err(|e| e.to_string())?;
    let got = skeleton::rebuild(skeleton, edited, &mut f)?;
    f.sync_all().map_err(|e| e.to_string())?;
    Ok(got == sha256)
}

pub fn run(args: &[String]) -> Result<()> {
    let db = open(&get(args, "--db").context("--db required")?)?;
    let out = PathBuf::from(get(args, "--out").context("--out required")?);
    if out.exists() {
        bail!("--out exists and is not overwritten: {}", out.display());
    }
    let path = get(args, "--path").map(|p| absolute(&p));
    let sha256 = match (get(args, "--sha256"), &path) {
        (Some(s), _) => s.to_lowercase(),
        (None, Some(p)) => db.query_row("SELECT sha256 FROM versions WHERE path=?1 ORDER BY replaced_unix DESC, mtime_ns DESC LIMIT 1",
                                        [p], |r| r.get::<_, String>(0)).optional()?
            .with_context(|| format!("the index records no earlier content of {p}"))?,
        (None, None) => bail!("--path or --sha256 required"),
    };
    let skeleton = load(&db, &sha256)?;
    let tried: Vec<String> = match get(args, "--from") {
        Some(f) => vec![f],
        None => candidates(&db, &sha256, &skeleton, path.as_deref())?,
    };
    let tmp = PathBuf::from(format!("{}.phindex-restore-{}", out.display(), std::process::id()));
    let mut reasons = Vec::new();
    for edited in tried.iter().filter(|p| Path::new(p).is_file()).take(CANDIDATES) {
        match attempt(&skeleton, &sha256, Path::new(edited), &tmp) {
            Ok(true) => {
                fs::rename(&tmp, &out)?;
                eprintln!("restored {sha256} ({} bytes) to {} with blocks from {edited}", skeleton.size, out.display());
                return Ok(());
            }
            Ok(false) => reasons.push(format!("{edited}: the blocks' ends match and the rebuilt SHA-256 does not")),
            Err(e) => reasons.push(format!("{edited}: {e}")),
        }
        let _ = fs::remove_file(&tmp);
    }
    if reasons.is_empty() {
        bail!("no live file to take the blocks of {sha256} from; name one with --from");
    }
    bail!("{sha256} was not rebuilt:\n  {}", reasons.join("\n  "))
}
