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

    // Two --only folders are both indexed, in one run.
    write(&root.join("p/one.txt"), "one");
    write(&root.join("q/two.txt"), "two");
    let out = Command::new(env!("CARGO_BIN_EXE_phindex"))
        .args(["--db", db.to_str().unwrap(), "--root", root.to_str().unwrap(), "--workers", "2",
               "--only", root.join("p").to_str().unwrap(), "--only", root.join("q").to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success());
    assert!(row(&db, &root.join("p/one.txt")).0.is_some() && row(&db, &root.join("q/two.txt")).0.is_some());

    // Moved away and back to the same path, unchanged: present again, not read.
    fs::rename(root.join("b/f2.txt"), root.join("f2.txt")).unwrap();
    run(&db, &root, &[]);
    assert_eq!(row(&db, &root.join("b/f2.txt")).1, 1);
    fs::rename(root.join("f2.txt"), root.join("b/f2.txt")).unwrap();
    let (read, _, _) = run(&db, &root, &[]);
    assert_eq!(read, 0);
    assert_eq!(row(&db, &root.join("b/f2.txt")).1, 0, "a returned file is no longer gone");
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
    assert!(md.is_some(), "the engine reads .txt files");
    assert!(prof_err.is_some(), "the refused DNG grammar leaves a reason, not a value");
    let n: i64 = c.query_row("SELECT count(*) FROM metadata", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn previous_revision_rows_are_upgraded_reading_only_image_candidates() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    write(&root.join("note.txt"), "not an image");
    write(&root.join("pseudo.dng"), "a row marked as a DNG below");
    // SOI, DQT, SOF0, SOS, two bytes of scan, EOI.
    let jpeg = [0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x03, 0x00, 0xFF, 0xC0, 0x00, 0x03, 0x08, 0xFF, 0xDA, 0x00, 0x03, 0x00, 0x11,
                0x22, 0xFF, 0xD9];
    fs::write(root.join("pic.jpg"), jpeg).unwrap();
    assert_eq!(run(&db, &root, &[]).0, 3);
    let c = Connection::open(&db).unwrap();
    let image: Option<String> =
        c.query_row("SELECT image_data FROM files WHERE path LIKE '%pic.jpg'", [], |r| r.get(0)).unwrap();
    assert!(image.is_some(), "a JPEG gets image data");
    let rev: String = c.query_row("SELECT ids_rev FROM files LIMIT 1", [], |r| r.get(0)).unwrap();
    let previous = rev.strip_suffix("; image-data/1").expect("current revision").to_string();
    c.execute("UPDATE files SET ids_rev=?1, image_data=NULL, image_err=NULL", [&previous]).unwrap();
    c.execute("UPDATE files SET dng_blocks='stand-in' WHERE path LIKE '%pseudo.dng'", []).unwrap();
    // An older Rust build gave this reason instead of walking a file over 1 GiB.
    let walked: String = c.query_row("SELECT dngsem_err FROM files WHERE path LIKE '%note.txt'", [], |r| r.get(0)).unwrap();
    c.execute("UPDATE files SET dngsem_err='file over the in-memory limit' WHERE path LIKE '%note.txt'", []).unwrap();
    drop(c);

    assert_eq!(run(&db, &root, &[]).0, 1, "only the JPEG is read again");
    let c = Connection::open(&db).unwrap();
    let rows: Vec<(String, String, Option<String>, Option<String>)> = c
        .prepare("SELECT path, ids_rev, image_data, image_err FROM files ORDER BY path").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap()
        .collect::<rusqlite::Result<_>>().unwrap();
    for (path, r, data, err) in &rows {
        assert_eq!(r, &rev, "{path} is at the current revision");
        match path.rsplit('/').next().unwrap() {
            "note.txt" => {
                assert_eq!(err.as_deref(), Some("not a TIFF-family file or JPEG"));
                let again: String = c.query_row("SELECT dngsem_err FROM files WHERE path=?1", [path], |r| r.get(0)).unwrap();
                assert_eq!(again, walked, "the tiffgraph walk is redone without a full read");
            }
            "pseudo.dng" => assert!(err.as_deref().unwrap().starts_with("DNG")),
            _ => assert_eq!(data, &image),
        }
    }
    assert_eq!(run(&db, &root, &[]).0, 0, "nothing left to upgrade");
}

/// Metadata is attempted only for extensions the engine reads: a Capture One
/// `.cop` cache proxy and an `.ini` get neither digest nor error nor record. An
/// index written before that rule loses those rows' metadata at the next
/// update, without reading anything again.
#[test]
fn metadata_only_for_extensions_the_engine_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    write(&root.join("note.txt"), "a text file");
    write(&root.join("Cache/a.DNG.cop"), "a cache proxy");
    write(&root.join("settings.ini"), "[x]\ny=1\n");
    assert_eq!(run(&db, &root, &[]).0, 3);
    let md = |c: &Connection| -> Vec<(String, bool, bool, bool)> {
        c.prepare("SELECT path, md_digest IS NOT NULL, md_err IS NOT NULL, path IN (SELECT path FROM metadata)
                   FROM files ORDER BY path").unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?.rsplit('/').next().unwrap().to_string(), r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap().collect::<rusqlite::Result<_>>().unwrap()
    };
    let want = vec![("a.DNG.cop".to_string(), false, false, false), ("note.txt".to_string(), true, false, true),
                    ("settings.ini".to_string(), false, false, false)];
    let c = Connection::open(&db).unwrap();
    assert_eq!(md(&c), want);
    // As an older build left them: an error for one, a record for the other.
    c.execute("UPDATE files SET md_err='metadata extraction failed: \"unsupported file type: ini\"' WHERE path LIKE '%.ini'", []).unwrap();
    c.execute("UPDATE files SET md_digest='ab' WHERE path LIKE '%.cop'", []).unwrap();
    let (path, rec): (String, Vec<u8>) =
        c.query_row("SELECT path, record_deflate FROM metadata", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    let cop: String = c.query_row("SELECT path FROM files WHERE path LIKE '%.cop'", [], |r| r.get(0)).unwrap();
    c.execute("INSERT OR REPLACE INTO metadata VALUES (?1, 'exiftool-rs (via phidentify)', ?2)", rusqlite::params![cop, rec]).unwrap();
    assert_ne!(path, cop);
    drop(c);
    assert_eq!(run(&db, &root, &[]).0, 0, "nothing is read again");
    assert_eq!(md(&Connection::open(&db).unwrap()), want);
}
