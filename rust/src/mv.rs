//! `phindex mv`: move a file, a folder's subtree or a listed set of files to
//! other paths, with the index following.
//!
//!   phindex mv --db INDEX.sqlite [--files] [--force] SRC DST
//!   phindex mv --db INDEX.sqlite [--files] [--force] --list DIFF.tsv --to DIR [--limit N] [--prune-empty] [--journal FILE]
//!   phindex mv undo --journal FILE --db INDEX.sqlite
//!
//! In every form the index rows move to the new paths with their identities,
//! metadata records and earlier versions, so the next indexing run reads
//! nothing again.
//!
//! Without `--files` only the index is changed, for files that were moved by
//! other means. Each file must then already be at its new path with the size
//! and modification time the index has for it; one that is not is refused
//! and left as it was, unless `--force` says to move its row anyway. With
//! `--files` the files are moved on disk as well, by renaming and never by
//! copying, so both paths must be on one volume. A file already at its new
//! path is recognised and only its row is moved.
//!
//! `SRC DST` moves one file, or a folder with everything under it. DST is the
//! whole new path, not a folder to move into, and must not exist; a folder is
//! moved by one rename and is not merged into another.
//!
//! `--list` takes the list `phindex diff` writes and moves each file of it to
//! the same relative path under `--to`. Every file is checked against the
//! list (size, modification time, a free destination); with `--files` each
//! rename is written to the journal, `LIST.journal` unless named, before it is
//! made, and `mv undo` replays that journal backwards. `--limit N` takes only
//! the first N files, for a trial. `--prune-empty` afterwards removes the
//! folders the moves left empty.
use crate::conform::{self, How, Step, execute, fields, get, open_index, repath, stat, step};
use crate::diff::LIST;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: phindex mv --db INDEX.sqlite [--files] [--force] SRC DST\n       \
                     phindex mv --db INDEX.sqlite [--files] [--force] --list DIFF.tsv --to DIR [--limit N] [--prune-empty] [--journal FILE]\n       \
                     phindex mv undo --journal FILE --db INDEX.sqlite";

pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("undo") => conform::undo(&args[1..]),
        Some(_) if args.iter().any(|a| a == "--list") => listed(args),
        Some(_) => direct(args),
        None => bail!("{USAGE}"),
    }
}

/// PATH made absolute as the index spells paths: its nearest existing ancestor
/// resolved, the rest appended. The path itself need not exist.
fn absolute(path: &str) -> Result<String> {
    let p = if Path::new(path).is_absolute() { PathBuf::from(path) } else { std::env::current_dir()?.join(path) };
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut base = p.as_path();
    loop {
        // The leaf is never resolved: a link named as SRC or DST is itself what is meant.
        if !rest.is_empty() && let Ok(real) = fs::canonicalize(base) {
            let full = rest.iter().rev().fold(real, |acc, c| acc.join(c));
            return full.to_str().map(|s| s.trim_end_matches('/').to_string()).context("a path that is not UTF-8");
        }
        match (base.file_name(), base.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                base = parent;
            }
            _ => bail!("{path}: not a path that can be resolved"),
        }
    }
}

/// The range of paths under a folder: [dir/, dir0).
fn under(dir: &str) -> (String, String) {
    (format!("{dir}/"), format!("{dir}0"))
}

fn direct(args: &[String]) -> Result<()> {
    let db_path = get(args, "--db").with_context(|| format!("--db required\n{USAGE}"))?;
    let (files, force) = (args.iter().any(|a| a == "--files"), args.iter().any(|a| a == "--force"));
    let mut names = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => i += 1,
            "--files" | "--force" => {}
            a if a.starts_with("--") => bail!("{a}: not an option of mv\n{USAGE}"),
            a => names.push(a),
        }
        i += 1;
    }
    let [src, dst] = names.as_slice() else { bail!("{USAGE}") };
    let (src, dst) = (absolute(src)?, absolute(dst)?);
    if src == dst {
        bail!("{src}: source and destination are the same path");
    }
    if dst.starts_with(&format!("{src}/")) || src.starts_with(&format!("{dst}/")) {
        bail!("one of {src} and {dst} is inside the other");
    }
    let mut db = open_index(&db_path)?;
    let row: Option<(i64, i64)> = {
        let mut st = db.prepare("SELECT size, mtime_ns FROM files WHERE path=?1 AND gone=0")?;
        let mut q = st.query([&src])?;
        match q.next()? { Some(r) => Some((r.get(0)?, r.get(1)?)), None => None }
    };
    let (lo, hi) = under(&src);
    let (dlo, dhi) = under(&dst);
    let live = |db: &Connection, lo: &str, hi: &str| -> Result<i64> {
        Ok(db.query_row("SELECT count(*) FROM files WHERE gone=0 AND path>=?1 AND path<?2", [lo, hi], |r| r.get(0))?)
    };

    if let Some((size, mtime)) = row {
        // One file.
        let moved = match step(&db, &src, &dst, size, mtime)? {
            Ok(Step::Rename) if files => {
                if let Some(p) = Path::new(&dst).parent() {
                    fs::create_dir_all(p)?;
                }
                fs::rename(&src, &dst).with_context(|| format!("renaming {src} to {dst}"))?;
                true
            }
            Ok(Step::Rename) if force => false,
            Ok(Step::Rename) => bail!("{src} is still there and {dst} is not: move the file first, pass --files to move it, \
                                       or --force to move the index row anyway"),
            Ok(Step::Made) => false,
            Err(_) if force && !files => false,
            Err(e) => bail!("{e}{}", if files { "" } else { " (--force moves the index row anyway)" }),
        };
        if db.query_row("SELECT count(*) FROM files WHERE path=?1 AND gone=0", [&dst], |r| r.get::<_, i64>(0))? > 0 {
            bail!("the index lists a file at {dst}; run phindex on the tree first");
        }
        let tx = db.transaction()?;
        repath(&tx, &src, &dst)?;
        tx.commit()?;
        eprintln!("done: 1 index row moved{}", if moved { ", the file renamed" } else { "; no file was touched" });
        return Ok(());
    }

    // A folder: every live row under it.
    let rows: Vec<(String, i64, i64)> = db.prepare("SELECT path, size, mtime_ns FROM files WHERE gone=0 AND path>=?1 AND path<?2 ORDER BY path")?
        .query_map([&lo, &hi], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        bail!("the index lists no file at or under {src}");
    }
    if live(&db, &dlo, &dhi)? > 0 {
        bail!("the index lists files under {dst}; run phindex on the tree first, or move its files with mv --list");
    }
    let there = Path::new(&src).symlink_metadata().is_ok();
    let mut renamed = false;
    if files && there {
        if Path::new(&dst).symlink_metadata().is_ok() {
            bail!("{dst} exists: a folder is moved by one rename, never merged into another");
        }
        if let Some(p) = Path::new(&dst).parent() {
            fs::create_dir_all(p)?;
        }
        fs::rename(&src, &dst).with_context(|| format!("renaming {src} to {dst} (a folder moves only within one volume)"))?;
        renamed = true;
    } else {
        // The files were moved by other means: each must be at its new path as the index has it.
        let mut amiss = 0u64;
        for (path, size, mtime) in &rows {
            let to = format!("{dst}{}", &path[src.len()..]);
            if stat(&to).map(|s| (s.0, s.1)) != Some((*size, *mtime)) {
                amiss += 1;
                if amiss <= 10 {
                    eprintln!("not in place: {to}");
                }
            }
        }
        if amiss > 0 && !(force && !files) {
            bail!("{amiss} of {} files are not under {dst} as the index has them; nothing was changed{}", rows.len(),
                  if files { " (the source folder is not there to move)" } else { ": move them first, pass --files to move them, \
                  or --force to move the index rows anyway" });
        }
        if amiss > 0 {
            eprintln!("warning: {amiss} of {} files are not under {dst} as the index has them; rows moved because of --force", rows.len());
        }
    }
    let tx = db.transaction()?;
    let n = src.len() as i64;
    tx.execute("DELETE FROM files WHERE gone=1 AND path>=?1 AND path<?2", [&dlo, &dhi])?;
    let moved = tx.execute("UPDATE files SET path = ?3 || substr(path, ?4 + 1) WHERE gone=0 AND path>=?1 AND path<?2", params![lo, hi, dst, n])?;
    tx.execute("DELETE FROM metadata WHERE path>=?1 AND path<?2", [&dlo, &dhi])?;
    tx.execute("UPDATE metadata SET path = ?3 || substr(path, ?4 + 1) WHERE path>=?1 AND path<?2
                AND NOT EXISTS (SELECT 1 FROM files f WHERE f.path = metadata.path)", params![lo, hi, dst, n])?;
    tx.execute("UPDATE OR IGNORE versions SET path = ?3 || substr(path, ?4 + 1) WHERE path>=?1 AND path<?2
                AND NOT EXISTS (SELECT 1 FROM files f WHERE f.path = versions.path)", params![lo, hi, dst, n])?;
    tx.execute("DELETE FROM versions WHERE path>=?1 AND path<?2 AND NOT EXISTS (SELECT 1 FROM files f WHERE f.path = versions.path)", [&lo, &hi])?;
    // The folders, with their inodes and times, so an unchanged one is not listed again.
    tx.execute("DELETE FROM dirs WHERE path=?3 OR (path>=?1 AND path<?2)", params![dlo, dhi, dst])?;
    tx.execute("UPDATE dirs SET path = ?3 || substr(path, ?4 + 1) WHERE gone=0 AND (path=?5 OR (path>=?1 AND path<?2))", params![lo, hi, dst, n, src])?;
    tx.commit()?;
    eprintln!("done: {moved} index rows moved from under {src} to {dst}{}", if renamed { ", the folder renamed" } else { "; no file was touched" });
    Ok(())
}

/// The folder both paths are under.
fn common(a: &str, b: &str) -> String {
    let shared: PathBuf = Path::new(a).components().zip(Path::new(b).components()).take_while(|(x, y)| x == y).map(|(x, _)| x).collect();
    shared.to_str().unwrap_or("/").trim_end_matches('/').to_string()
}

/// The files of a `phindex diff` list, each moved to the same relative path under --to.
fn listed(args: &[String]) -> Result<()> {
    let need = |flag: &str| get(args, flag).with_context(|| format!("{flag} required\n{USAGE}"));
    let (db, list, to) = (need("--db")?, need("--list")?, absolute(&need("--to")?)?);
    let journal = get(args, "--journal").unwrap_or_else(|| format!("{list}.journal"));
    let limit: Option<usize> = get(args, "--limit").map(|n| n.parse().context("--limit takes a number of files")).transpose()?;
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let how = How { files: has("--files"), force: has("--force"), prune: has("--prune-empty") };
    if how.force && how.files {
        bail!("--force is for moving index rows without --files; with --files every file is checked and moved");
    }
    let text = fs::read_to_string(&list).with_context(|| format!("no list at {list}"))?;
    let mut lines = text.lines();
    let head = fields(lines.next().unwrap_or(""), 3, &list)?;
    if head[0] != LIST || head[1] != "1" {
        bail!("{list} is not a phindex diff list of format 1");
    }
    let from = head[2].clone();
    if from == to || to.starts_with(&format!("{from}/")) || from.starts_with(&format!("{to}/")) {
        bail!("{from}, the list's root, and {to} must be two folders, neither inside the other");
    }
    let root = common(&from, &to);
    if root.is_empty() {
        bail!("{from} and {to} share no folder");
    }
    let mut units = Vec::new();
    for line in lines.skip(1).take(limit.unwrap_or(usize::MAX)) {
        let f = fields(line, 5, &list)?;
        units.push((f[0].clone(), vec![(format!("{from}/{}", f[0]), format!("{to}/{}", f[0]), f[2].parse()?, f[3].parse()?)]));
    }
    execute(&db, &journal, &root, &units, how)
}
