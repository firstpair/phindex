//! `phindex conform`: give the aligned files of a tree the names they have in
//! another, by renaming them in place.
//!
//!   phindex conform plan  --report REPORT_DIR --db RIGHT.sqlite --out PLAN_DIR [--limit N]
//!   phindex conform apply --plan PLAN_DIR/plan.tsv --db RIGHT.sqlite [--journal FILE]
//!   phindex conform undo  --journal FILE --db RIGHT.sqlite
//!
//! `align` says which right unit each left unit corresponds to. Where that
//! unit sits in another folder or under another name, `plan` lists the renames
//! that would give it the left unit's folder and name: each matched right file
//! takes the path of the left file it was matched to, and a right member the
//! left unit lacks (a sidecar, say) follows with its own suffix. `plan` reads
//! the report and the right index and touches nothing.
//!
//! A unit is left alone, and listed in `skipped.tsv` with the reason, when the
//! rename is not plainly safe: its members went to more than one right unit
//! (`split`), a member has more than one candidate (`duplicates`), several
//! left units share the right unit (`target-shared`), a file is not read yet
//! (`not-indexed`), the index no longer lists a file (`not-in-index`), or a
//! destination is already a live path or claimed by another rename
//! (`destination-taken`). Names are compared without regard to case for that
//! last test, so a plan is safe on a case-insensitive volume. A destination
//! held by a file that is itself renamed away becomes free on the next round:
//! index, align and plan again.
//!
//! With `--limit N` the plan stops before the unit that would take it past N
//! files, for a trial on a few units. Units stay whole, so it may hold fewer
//! than N files; the units after that point are neither planned nor listed
//! as skipped.
//!
//! `apply` renames, never copies: a file's bytes, inode and times are as
//! before, so only its name changes. A unit is renamed whole or not at all.
//! Before any rename of a unit, every one of its files must still have the
//! size and modification time of the plan, and every destination must be
//! free. Each rename is written to the journal before it is made, and the
//! right index's rows are moved to the new paths with their identities, so
//! the next indexing run reads nothing again. `apply` may be run again after
//! an interruption; renames already made are recognised.
//!
//! `undo` replays the journal backwards with the same checks, moves the index
//! rows back, and removes the folders `apply` created if they are empty. A
//! content never changes, so no skeleton is needed to undo a rename.
use crate::align::unit_key;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, Transaction, params};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

const PLAN: &str = "phindex-conform-plan";
const JOURNAL: &str = "phindex-conform-journal";
const USAGE: &str = "usage: phindex conform plan --report REPORT_DIR --db RIGHT.sqlite --out PLAN_DIR [--limit N]\n       \
                     phindex conform apply --plan PLAN.tsv --db RIGHT.sqlite [--journal FILE]\n       \
                     phindex conform undo --journal FILE --db RIGHT.sqlite";

fn get(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn need(args: &[String], flag: &str) -> Result<String> {
    get(args, flag).with_context(|| format!("{flag} required\n{USAGE}"))
}

pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("plan") => plan(&args[1..]),
        Some("apply") => apply(&args[1..]),
        Some("undo") => undo(&args[1..]),
        _ => bail!("{USAGE}"),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn fields(line: &str, n: usize, file: &str) -> Result<Vec<String>> {
    let f: Vec<String> = line.split('\t').map(String::from).collect();
    if f.len() != n {
        bail!("{file}: a line with {} fields where {n} are expected (a name with a tab in it?): {line}", f.len());
    }
    Ok(f)
}

fn plan(args: &[String]) -> Result<()> {
    let (report, db, out) = (need(args, "--report")?, need(args, "--db")?, need(args, "--out")?);
    let limit: Option<u64> = get(args, "--limit").map(|n| n.parse().context("--limit takes a number of files")).transpose()?;
    let read = |name: &str| fs::read_to_string(Path::new(&report).join(name)).with_context(|| format!("{report}/{name}: not an align report?"));
    let summary: serde_json::Value = serde_json::from_str(&read("summary.json")?)?;
    let root = summary["right"]["root"].as_str().context("summary.json names no right root")?.to_string();
    let plan_path = Path::new(&out).join("plan.tsv");
    if plan_path.exists() {
        bail!("{} exists: a plan is not overwritten, since a journal may refer to it", plan_path.display());
    }
    if !Path::new(&db).is_file() {
        bail!("no index at {db}");
    }

    // The right tree as its index has it now: live paths, and units as align forms them.
    // An idle index is opened immutable, which writes nothing beside it.
    let c = if Path::new(&format!("{db}-wal")).exists() {
        Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
    } else {
        let real = fs::canonicalize(&db)?.to_str().context("non-UTF-8 --db")?.replace('%', "%25").replace('?', "%3f").replace('#', "%23");
        Connection::open_with_flags(format!("file:{real}?immutable=1"), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
    }?;
    let mut rows: HashMap<String, (i64, i64, Option<String>)> = HashMap::new();
    let mut right_units: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut live_lower: HashSet<String> = HashSet::new();
    {
        let mut st = c.prepare("SELECT path, size, mtime_ns, sha256 FROM files WHERE gone=0 AND path >= ?1 AND path < ?2 ORDER BY path")?;
        let mut q = st.query(params![format!("{root}/"), format!("{root}0")])?;
        while let Some(r) = q.next()? {
            let path: String = r.get(0)?;
            let rel = path[root.len() + 1..].to_string();
            live_lower.insert(rel.to_lowercase());
            if !rel.rsplit('/').next().unwrap_or(&rel).starts_with('.') {
                right_units.entry(unit_key(&rel)).or_default().push(rel.clone());
            }
            rows.insert(rel, (r.get(1)?, r.get(2)?, r.get(3)?));
        }
    }
    if rows.is_empty() {
        bail!("{db} lists no files under {root}, the report's right root");
    }

    let mut matched: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    for line in read("files.tsv")?.lines().skip(1) {
        let f = fields(line, 4, "files.tsv")?;
        if !f[1].is_empty() {
            matched.entry(unit_key(&f[0])).or_default().push((f[0].clone(), f[1].clone()));
        }
    }
    let all_matched: HashSet<&str> = matched.values().flatten().map(|(_, r)| r.as_str()).collect();
    let units_text = read("units.tsv")?;
    let mut units = Vec::new();
    let mut sharing: HashMap<String, usize> = HashMap::new();
    for line in units_text.lines().skip(1) {
        let f = fields(line, 6, "units.tsv")?;
        if !f[4].is_empty() {
            *sharing.entry(f[4].clone()).or_default() += 1;
            units.push(f);
        }
    }

    fs::create_dir_all(&out)?;
    let mut plan_tsv = BufWriter::new(fs::File::create(&plan_path)?);
    let mut skipped_tsv = BufWriter::new(fs::File::create(Path::new(&out).join("skipped.tsv"))?);
    writeln!(plan_tsv, "{PLAN}\t1\t{root}")?;
    writeln!(plan_tsv, "unit\tfrom\tto\tsize\tmtime_ns\tsha256\twhy")?;
    writeln!(skipped_tsv, "left_unit\tright_unit\treason\tfiles")?;
    let (mut from_taken, mut to_taken): (HashSet<String>, HashSet<String>) = (HashSet::new(), HashSet::new());
    let (mut n_units, mut n_files, mut in_place) = (0u64, 0u64, 0u64);
    let mut limited = false;
    let mut by_why: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_reason: BTreeMap<&'static str, u64> = BTreeMap::new();
    for u in &units {
        let (left_unit, status, flags, right_unit) = (&u[0], &u[2], &u[3], &u[4]);
        let key = |text: &str| { let (d, s) = text.rsplit_once('/').unwrap_or(("", text)); (d.to_string(), s.to_string()) };
        let ((dir, stem), (rdir, rstem)) = (key(left_unit), key(right_unit));
        let target = (rdir.clone(), rstem.clone());
        let has = |flag: &str| flags.split(',').any(|f| f == flag);
        let mut moves: Vec<(String, String)> = Vec::new();
        let mut matched_right: HashSet<&str> = HashSet::new();
        for (l, r) in matched.get(&(dir.clone(), stem.clone())).into_iter().flatten() {
            matched_right.insert(r);
            if l != r {
                moves.push((r.clone(), l.clone()));
            }
        }
        if (&dir, &stem) != (&rdir, &rstem) {
            // Right members the left unit lacks follow the unit, keeping their suffixes.
            for extra in right_units.get(&target).into_iter().flatten().filter(|e| !matched_right.contains(e.as_str())) {
                let name = extra.rsplit('/').next().unwrap_or(extra);
                moves.push((extra.clone(), join(&dir, &format!("{stem}{}", &name[rstem.len()..]))));
            }
        }
        if moves.is_empty() {
            in_place += 1;
            continue;
        }
        let lent = moves.iter().any(|(f, _)| !matched_right.contains(f.as_str()) && all_matched.contains(f.as_str()));
        let mut local: HashSet<String> = HashSet::new();
        let reason: Option<&'static str> = if status == "not-indexed" {
            Some("not-indexed")
        } else if has("split") {
            Some("split")
        } else if has("duplicates") {
            Some("duplicates")
        } else if sharing[right_unit] > 1 || lent || moves.iter().any(|(f, _)| from_taken.contains(f)) {
            Some("target-shared")
        } else if moves.iter().any(|(f, _)| !rows.contains_key(f)) {
            Some("not-in-index")
        } else if moves.iter().any(|(f, _)| rows[f].2.is_none()) {
            Some("not-indexed")
        } else if moves.iter().any(|(f, t)| {
            let (fl, tl) = (f.to_lowercase(), t.to_lowercase());
            rows.contains_key(t) || (live_lower.contains(&tl) && tl != fl) || to_taken.contains(&tl) || !local.insert(tl)
        }) {
            Some("destination-taken")
        } else {
            None
        };
        if let Some(reason) = reason {
            *by_reason.entry(reason).or_default() += 1;
            writeln!(skipped_tsv, "{left_unit}\t{right_unit}\t{reason}\t{}", moves.len())?;
            continue;
        }
        // A trial plan: whole units, in the report's order, up to the limit.
        if limit.is_some_and(|n| n_files + moves.len() as u64 > n) {
            limited = true;
            break;
        }
        let why: Vec<&str> = ["moved", "renamed"].into_iter().filter(|f| has(f)).collect();
        let why = if why.is_empty() { "name".to_string() } else { why.join(",") };
        n_units += 1;
        *by_why.entry(why.clone()).or_default() += 1;
        for (f, t) in moves {
            let (size, mtime, sha) = &rows[&f];
            writeln!(plan_tsv, "{left_unit}\t{f}\t{t}\t{size}\t{mtime}\t{}\t{why}", sha.as_deref().unwrap_or(""))?;
            n_files += 1;
            to_taken.insert(t.to_lowercase());
            from_taken.insert(f);
        }
    }
    plan_tsv.flush()?;
    skipped_tsv.flush()?;
    let out_summary = serde_json::json!({
        "root": root,
        "units_to_rename": n_units,
        "files_to_rename": n_files,
        "units_to_rename_by_cause": by_why,
        "units_already_in_place": in_place,
        "units_skipped_by_reason": by_reason,
        "limit": limit,
        "stopped_at_the_limit": limited,
        "plan": plan_path.to_str(),
    });
    println!("{}", serde_json::to_string_pretty(&out_summary)?);
    Ok(())
}

/// A file as the plan or the journal states it: (size, mtime_ns, device, inode) if it is a regular file.
fn stat(path: &str) -> Option<(i64, i64, i64, i64)> {
    match crate::lstat(Path::new(path)) {
        Some((false, id, dev)) => Some((id.size, id.mtime_ns, dev, id.ino)),
        _ => None,
    }
}

enum Step {
    Rename,
    Made,
}

/// What to do for one rename FROM -> TO of a file of SIZE and MTIME, or why not.
fn step(db: &Connection, from: &str, to: &str, size: i64, mtime: i64) -> Result<Result<Step, String>> {
    let row = |path: &str| -> Result<Option<(i64, i64, bool)>> {
        let mut st = db.prepare_cached("SELECT size, mtime_ns, gone FROM files WHERE path=?1")?;
        let mut q = st.query([path])?;
        Ok(match q.next()? { Some(r) => Some((r.get(0)?, r.get(1)?, r.get(2)?)), None => None })
    };
    let listed = |r: Option<(i64, i64, bool)>| matches!(r, Some((s, m, false)) if s == size && m == mtime);
    Ok(match (stat(from), stat(to)) {
        (Some(f), t) => {
            if (f.0, f.1) != (size, mtime) {
                Err(format!("{from} has changed since it was listed"))
            } else if Path::new(to).symlink_metadata().is_ok() && t.map(|t| (t.2, t.3)) != Some((f.2, f.3)) {
                Err(format!("{to} exists"))
            } else if from != to && t.is_some() && listed(row(to)?) && row(from)?.is_none() {
                Ok(Step::Made) // a change of letter case only, already made: both names reach the file
            } else if !listed(row(from)?) {
                Err(format!("the index does not list {from} as it is; run phindex on the tree first"))
            } else if from != to && matches!(row(to)?, Some((_, _, false))) {
                Err(format!("the index lists a file at {to}; run phindex on the tree first"))
            } else {
                Ok(Step::Rename)
            }
        }
        (None, Some(t)) if (t.0, t.1) == (size, mtime) && Path::new(from).symlink_metadata().is_err() => Ok(Step::Made),
        (None, _) => Err(format!("{from} is not there")),
    })
}

/// The index row of FROM, with its metadata and earlier versions, moved to TO.
fn repath(tx: &Transaction, from: &str, to: &str) -> Result<()> {
    if tx.query_row("SELECT count(*) FROM files WHERE path=?1 AND gone=0", [from], |r| r.get::<_, i64>(0))? == 0 {
        return Ok(()); // already moved
    }
    tx.execute("DELETE FROM files WHERE path=?1 AND gone=1", [to])?;
    tx.execute("UPDATE files SET path=?2 WHERE path=?1", [from, to])?;
    tx.execute("DELETE FROM metadata WHERE path=?1", [to])?;
    tx.execute("UPDATE metadata SET path=?2 WHERE path=?1", [from, to])?;
    tx.execute("UPDATE OR IGNORE versions SET path=?2 WHERE path=?1", [from, to])?;
    tx.execute("DELETE FROM versions WHERE path=?1", [from])?;
    Ok(())
}

fn open_index(db_path: &str) -> Result<Connection> {
    if !Path::new(db_path).is_file() {
        bail!("no index at {db_path}");
    }
    let db = Connection::open(db_path)?;
    db.execute_batch("PRAGMA journal_mode=WAL;")?;
    db.busy_timeout(std::time::Duration::from_secs(30))?;
    crate::migrate(&db)?;
    Ok(db)
}

fn apply(args: &[String]) -> Result<()> {
    let (plan_path, db_path) = (need(args, "--plan")?, need(args, "--db")?);
    let journal_path = get(args, "--journal").unwrap_or_else(|| format!("{plan_path}.journal"));
    let text = fs::read_to_string(&plan_path).with_context(|| format!("no plan at {plan_path}"))?;
    let mut lines = text.lines();
    let head = fields(lines.next().unwrap_or(""), 3, &plan_path)?;
    if head[0] != PLAN || head[1] != "1" {
        bail!("{plan_path} is not a conform plan of format 1");
    }
    let root = head[2].clone();
    if !Path::new(&root).is_dir() {
        bail!("{root}, the plan's root, is not a folder here");
    }
    // Units, in the plan's order: (unit, [(from, to, size, mtime_ns)]).
    let mut units: Vec<(String, Vec<(String, String, i64, i64)>)> = Vec::new();
    for line in lines.skip(1) {
        let f = fields(line, 7, &plan_path)?;
        if units.last().is_none_or(|(u, _)| *u != f[0]) {
            units.push((f[0].clone(), Vec::new()));
        }
        units.last_mut().unwrap().1.push((format!("{root}/{}", f[1]), format!("{root}/{}", f[2]), f[3].parse()?, f[4].parse()?));
    }
    let mut db = open_index(&db_path)?;
    let known = fs::read_to_string(&journal_path).unwrap_or_default();
    let mut journal = fs::OpenOptions::new().create(true).append(true).open(&journal_path)?;
    match known.lines().next() {
        None => writeln!(journal, "{JOURNAL}\t1\t{root}")?,
        Some(h) if h == format!("{JOURNAL}\t1\t{root}") => {}
        Some(_) => bail!("{journal_path} is not the journal of a plan for {root}"),
    }
    let total: usize = units.iter().map(|(_, m)| m.len()).sum();
    eprintln!("conform: {total} files in {} units under {root}; journal {journal_path}", units.len());
    let (mut renamed, mut made, mut refused_units, mut refused_files, mut done_units) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (t0, mut last) = (Instant::now(), Instant::now());
    for (unit, members) in &units {
        let mut steps = Vec::new();
        let mut refusal = None;
        for (from, to, size, mtime) in members {
            match step(&db, from, to, *size, *mtime)? {
                Ok(s) => steps.push(s),
                Err(e) => { refusal = Some(e); break; }
            }
        }
        if let Some(e) = refusal {
            refused_units += 1;
            refused_files += members.len() as u64;
            if refused_units <= 20 {
                eprintln!("not renamed, {unit}: {e}");
            }
            continue;
        }
        // The journal first: folders to create, then the renames, on disk before any is made.
        let mut folders: Vec<String> = Vec::new();
        for ((_, to, _, _), _) in members.iter().zip(&steps).filter(|(_, s)| matches!(s, Step::Rename)) {
            let mut missing = Vec::new();
            let mut d = Path::new(to).parent();
            while let Some(p) = d.filter(|p| p.starts_with(&root) && !p.exists()) {
                missing.push(p.to_str().context("a path that is not UTF-8")?.to_string());
                d = p.parent();
            }
            for m in missing.into_iter().rev().filter(|m| !folders.contains(m)).collect::<Vec<_>>() {
                folders.push(m);
            }
        }
        for f in &folders {
            writeln!(journal, "mkdir\t{f}")?;
        }
        for ((from, to, size, mtime), _) in members.iter().zip(&steps).filter(|(_, s)| matches!(s, Step::Rename)) {
            writeln!(journal, "move\t{from}\t{to}\t{size}\t{mtime}")?;
        }
        journal.sync_data()?;
        for f in &folders {
            fs::create_dir(f).with_context(|| format!("creating {f}"))?;
        }
        for ((from, to, _, _), s) in members.iter().zip(&steps) {
            match s {
                Step::Rename => {
                    fs::rename(from, to).with_context(|| format!("renaming {from} to {to}; stopped, run apply again or undo"))?;
                    renamed += 1;
                }
                Step::Made => made += 1,
            }
        }
        let tx = db.transaction()?;
        for (from, to, _, _) in members {
            repath(&tx, from, to)?;
        }
        tx.commit()?;
        done_units += 1;
        if last.elapsed().as_secs() >= 30 {
            last = Instant::now();
            eprintln!("+{}s {}/{} units, {renamed} files renamed, {refused_units} units refused", t0.elapsed().as_secs(),
                      done_units + refused_units, units.len());
        }
    }
    eprintln!("done: {renamed} files renamed in place, {made} already renamed by an earlier run, {done_units} units; \
               {refused_units} units refused ({refused_files} files), left as they were");
    if refused_units > 0 {
        bail!("{refused_units} units were refused");
    }
    Ok(())
}

fn undo(args: &[String]) -> Result<()> {
    let (journal_path, db_path) = (need(args, "--journal")?, need(args, "--db")?);
    let text = fs::read_to_string(&journal_path).with_context(|| format!("no journal at {journal_path}"))?;
    let head = fields(text.lines().next().unwrap_or(""), 3, &journal_path)?;
    if head[0] != JOURNAL || head[1] != "1" {
        bail!("{journal_path} is not a conform journal of format 1");
    }
    let mut db = open_index(&db_path)?;
    let lines: Vec<&str> = text.lines().skip(1).collect();
    let (mut back, mut already, mut refused, mut folders) = (0u64, 0u64, 0u64, 0u64);
    for chunk in lines.rchunks(500) {
        let tx = db.transaction()?;
        for line in chunk.iter().rev() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["mkdir", dir] => folders += fs::remove_dir(dir).is_ok() as u64, // only if empty
                ["move", from, to, size, mtime] => match step(&tx, to, from, size.parse()?, mtime.parse()?)? {
                    Ok(Step::Rename) => {
                        if let Some(p) = Path::new(from).parent() {
                            fs::create_dir_all(p)?;
                        }
                        fs::rename(to, from).with_context(|| format!("renaming {to} back to {from}; stopped, run undo again"))?;
                        repath(&tx, to, from)?;
                        back += 1;
                    }
                    Ok(Step::Made) => {
                        repath(&tx, to, from)?;
                        already += 1;
                    }
                    Err(e) => {
                        refused += 1;
                        if refused <= 20 {
                            eprintln!("not renamed back: {e}");
                        }
                    }
                },
                _ => bail!("{journal_path}: a line that is not understood: {line}"),
            }
        }
        tx.commit()?;
    }
    eprintln!("done: {back} files renamed back, {already} were already under their old names, {folders} empty folders removed; \
               {refused} refused, left as they were");
    if refused > 0 {
        bail!("{refused} renames were not undone");
    }
    Ok(())
}
