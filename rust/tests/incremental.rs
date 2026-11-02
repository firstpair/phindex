//! The phindex binary on disposable synthetic trees: what a rerun reads, the
//! quick directory walk, --full-stat, --only, gone files and moves.
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Run phindex and return (files read, files gone, files moved).
fn run(db: &Path, root: &Path, extra: &[&str]) -> (u64, u64, u64) {
    let out = Command::new(env!("CARGO_BIN_EXE_phindex"))
        .args(["--db", db.to_str().unwrap(), "--root", root.to_str().unwrap(), "--workers", "2"])
        .args(extra)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "phindex failed: {err}");
    let num = |line: &str, after: &str| -> u64 {
        let rest = &line[line.find(after).unwrap_or_else(|| panic!("{after} in {line}")) + after.len()..];
        rest.trim_start().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
    };
    let walked = err.lines().find(|l| l.starts_with("walked")).expect("walked line");
    let done = err.lines().find(|l| l.starts_with("done:")).expect("done line");
    let moved = walked.split(", ").find(|p| p.contains("moved or renamed")).unwrap();
    let gone = walked.split(", ").find(|p| p.ends_with(" gone")).unwrap();
    (num(done, "read"), num(gone, ""), num(moved, ""))
}

fn write(p: &Path, text: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn row(db: &Path, path: &Path) -> (Option<String>, i64) {
    let c = Connection::open(db).unwrap();
    c.query_row("SELECT sha256, gone FROM files WHERE path=?1", [path.to_str().unwrap()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
}

#[test]
fn reruns_read_only_what_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    for i in 0..3 {
        write(&root.join(format!("y2020/a/f{i}.txt")), &format!("file {i}"));
        write(&root.join(format!("y2021/b/g{i}.txt")), &format!("g {i}"));
    }
    assert_eq!(run(&db, &root, &[]).0, 6, "first run reads everything");
    assert_eq!(run(&db, &root, &[]).0, 0, "unchanged tree reads nothing");

    write(&root.join("y2021/c/deep/n.txt"), "new");
    assert_eq!(run(&db, &root, &[]).0, 1, "a new folder is found by the quick walk");

    // Rewriting a file in place leaves its directory's mtime alone: the quick walk
    // does not see it, --full-stat does.
    let f1 = root.join("y2020/a/f1.txt");
    let before = row(&db, &f1).0;
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::OpenOptions::new().append(true).open(&f1).unwrap();
    std::io::Write::write_all(&mut fs::OpenOptions::new().append(true).open(&f1).unwrap(), b" more").unwrap();
    assert_eq!(run(&db, &root, &[]).0, 0);
    assert_eq!(run(&db, &root, &["--full-stat"]).0, 1);
    assert_ne!(row(&db, &f1).0, before, "the rewritten file has a new SHA-256");

    fs::remove_file(root.join("y2021/b/g2.txt")).unwrap();
    let (read, gone, _) = run(&db, &root, &[]);
    assert_eq!((read, gone), (0, 1));
    assert_eq!(row(&db, &root.join("y2021/b/g2.txt")).1, 1);
    assert_eq!(run(&db, &root, &[]).1, 0, "a file already gone is not counted again");
}

#[test]
fn moves_keep_identities_and_only_limits_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    for i in 0..3 {
        write(&root.join(format!("a/f{i}.txt")), &format!("file {i}"));
    }
    run(&db, &root, &[]);
    let sha = row(&db, &root.join("a/f0.txt")).0;

    fs::rename(root.join("a"), root.join("b")).unwrap();
    let (read, gone, moved) = run(&db, &root, &[]);
    assert_eq!((read, gone, moved), (0, 3, 3), "a renamed folder is not read again");
    assert_eq!(row(&db, &root.join("b/f0.txt")).0, sha);

    write(&root.join("new/n.txt"), "new");
    fs::rename(root.join("b/f1.txt"), root.join("new/f1.txt")).unwrap();
    let (read, _, moved) = run(&db, &root, &["--only", root.join("new").to_str().unwrap()]);
    assert_eq!((read, moved), (1, 1), "--only reads the new file and carries the moved one");
    assert_eq!(row(&db, &root.join("b/f1.txt")).1, 1, "the move's source outside --only is marked gone");
}

#[test]
fn identities_are_recorded_separately() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    write(&root.join("note.txt"), "not a photo");
    run(&db, &root, &[]);
    let c = Connection::open(&db).unwrap();
    let col = |name: &str| -> Option<String> {
        c.query_row(&format!("SELECT {name} FROM files"), [], |r| r.get(0)).unwrap()
    };
    let (sha, blocks, lr, md, prof_err, pres) =
        (col("sha256").unwrap(), col("dng_blocks"), col("lr_digest"), col("md_digest"), col("dngsem_err"), col("preserved"));
    assert_eq!(sha.len(), 64);
    assert!(blocks.is_none() && lr.is_none() && pres.is_none(), "no DNG identity for a text file");
    assert!(md.is_some(), "metadata applies to any file");
    assert!(prof_err.is_some(), "the refused DNG grammar leaves a reason, not a value");
    let n: i64 = c.query_row("SELECT count(*) FROM metadata", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}
