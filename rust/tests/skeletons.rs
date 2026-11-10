//! Block marks, skeletons and earlier contents, on disposable synthetic trees:
//! what the indexer stores, what an edit leaves behind, `phindex restore`, and
//! the upgrade of an index written before block marks.
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn phindex(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_phindex")).args(args).output().unwrap()
}

/// Index the tree; returns the number of files read in full.
fn run(db: &Path, root: &Path) -> u64 {
    let out = phindex(&["--db", db.to_str().unwrap(), "--root", root.to_str().unwrap(), "--workers", "2"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "phindex failed: {err}");
    let done = err.lines().find(|l| l.starts_with("done: read ")).expect("done line");
    done["done: read ".len()..].split(' ').next().unwrap().parse().unwrap()
}

/// A little-endian TIFF: a 2,048-byte strip, an XMP packet (tag 700), a
/// 600-byte private tag (50740) and DNGVersion.
fn tiff(xmp: &str, fill: u8) -> Vec<u8> {
    let strip: Vec<u8> = (0..2048u32).map(|i| (i as u8).wrapping_mul(7).wrapping_add(fill)).collect();
    let blob: Vec<u8> = (0..600u32).map(|i| (i as u8).wrapping_mul(3)).collect();
    let entries: u32 = 9;
    let xmp_at = 8 + 2 + entries * 12 + 4;
    let blob_at = xmp_at + xmp.len() as u32 + (xmp.len() as u32 & 1);
    let strip_at = blob_at + blob.len() as u32;
    let mut out = vec![b'I', b'I', 42, 0];
    out.extend(8u32.to_le_bytes());
    out.extend((entries as u16).to_le_bytes());
    let mut tag = |t: u16, ty: u16, n: u32, v: u32| {
        out.extend(t.to_le_bytes());
        out.extend(ty.to_le_bytes());
        out.extend(n.to_le_bytes());
        out.extend(v.to_le_bytes());
    };
    tag(256, 4, 1, 32);
    tag(257, 4, 1, 32);
    tag(258, 3, 1, 16);
    tag(259, 3, 1, 1);
    tag(273, 4, 1, strip_at);
    tag(279, 4, 1, strip.len() as u32);
    tag(700, 1, xmp.len() as u32, xmp_at);
    tag(50706, 1, 4, u32::from_le_bytes([1, 4, 0, 0]));
    tag(50740, 1, blob.len() as u32, blob_at);
    out.extend(0u32.to_le_bytes());
    out.extend(xmp.as_bytes());
    if xmp.len() & 1 == 1 {
        out.push(0);
    }
    out.extend(&blob);
    out.extend(&strip);
    out
}

fn jpeg(note: &str) -> Vec<u8> {
    let mut out = vec![0xff, 0xd8];
    let mut seg = |marker: u8, body: &[u8]| {
        out.extend([0xff, marker]);
        out.extend(((body.len() + 2) as u16).to_be_bytes());
        out.extend(body);
    };
    seg(0xe1, format!("Exif\0\0{note}").as_bytes());
    seg(0xe2, &(0..700u32).map(|i| i as u8).collect::<Vec<_>>());
    out.extend([0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0]);
    out.extend((0..3000u32).map(|i| (i as u8) & 0x7f));
    out.extend([0xff, 0xd9]);
    out
}

fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// Replace a file as an editor does: a new file renamed over the old one.
fn replace(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp-edit");
    fs::write(&tmp, bytes).unwrap();
    fs::rename(&tmp, path).unwrap();
}

fn text(db: &Path, sql: &str, path: &Path) -> Option<String> {
    Connection::open(db).unwrap().query_row(sql, [path.to_str().unwrap()], |r| r.get(0)).unwrap()
}

fn count(db: &Path, sql: &str) -> i64 {
    Connection::open(db).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn block_marks_and_a_skeleton_per_content() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    let (raw, pic, sidecar, video) = (root.join("a/one.dng"), root.join("a/one.jpg"), root.join("a/one.xmp"), root.join("a/clip.mov"));
    put(&raw, &tiff("<x:xmpmeta>rating 0</x:xmpmeta>", 1));
    put(&root.join("b/copy.dng"), &tiff("<x:xmpmeta>rating 0</x:xmpmeta>", 1));
    put(&pic, &jpeg("caption"));
    put(&sidecar, b"<x:xmpmeta>rating 2</x:xmpmeta>");
    put(&video, &vec![5u8; (1 << 20) + 1]);
    assert_eq!(run(&db, &root), 5);
    let marks = |p: &Path| text(&db, "SELECT block_marks FROM files WHERE path=?1", p);
    let reason = |p: &Path| text(&db, "SELECT block_marks_err FROM files WHERE path=?1", p);
    assert_eq!(marks(&raw).unwrap().len(), 64);
    assert_eq!(marks(&raw), marks(&root.join("b/copy.dng")));
    assert_ne!(marks(&raw), marks(&pic));
    assert!(marks(&pic).is_some() && reason(&pic).is_none());
    assert_eq!((marks(&sidecar), reason(&sidecar).as_deref()), (None, Some("no skeleton walker for this format")));
    assert_eq!((marks(&video), reason(&video).as_deref()), (None, Some("no skeleton walker for this format")));
    // One skeleton per content: the two equal DNGs share one, the video has none.
    assert_eq!(count(&db, "SELECT count(*) FROM skeletons"), 3);
    assert_eq!(count(&db, "SELECT count(*) FROM files f WHERE EXISTS (SELECT 1 FROM skeletons s WHERE s.sha256 = f.sha256)"), 4);
    let record = text(&db, "SELECT record FROM skeletons WHERE sha256 = (SELECT sha256 FROM files WHERE path=?1)", &raw).unwrap();
    assert!(record.contains("\"format\": \"tiff\"") && record.contains("\"0/strips\"") && record.contains("\"0/tag50740\""), "{record}");
    assert_eq!(count(&db, "SELECT count(*) FROM versions"), 0);
    assert_eq!(run(&db, &root), 0);
}

#[test]
fn an_edited_file_is_restored_as_it_was() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    let raw = root.join("a/one.dng");
    let v0 = tiff("<x:xmpmeta>rating 0</x:xmpmeta>", 1);
    let v1 = tiff(&format!("<x:xmpmeta>rating 5{}</x:xmpmeta>", " ".repeat(200)), 1);
    let v2 = tiff(&format!("<x:xmpmeta>rating 5, exposure +0.50, blacks +12{}</x:xmpmeta>", " ".repeat(333)), 1);
    put(&raw, &v0);
    put(&root.join("a/other.txt"), b"another file");
    run(&db, &root);
    let marks = text(&db, "SELECT block_marks FROM files WHERE path=?1", &raw);
    let sha = |p: &Path| text(&db, "SELECT sha256 FROM files WHERE path=?1", p).unwrap();
    let sha0 = sha(&raw);
    replace(&raw, &v1);
    assert_eq!(run(&db, &root), 1);
    let sha1 = sha(&raw);
    replace(&raw, &v2);
    assert_eq!(run(&db, &root), 1);
    assert_eq!(text(&db, "SELECT block_marks FROM files WHERE path=?1", &raw), marks, "the edits kept the block marks");
    assert_eq!(count(&db, "SELECT count(*) FROM versions"), 2);
    assert_eq!(count(&db, "SELECT count(*) FROM skeletons"), 4, "three contents of the DNG and the text file");

    let listed = phindex(&["versions", "--db", db.to_str().unwrap(), "--path", raw.to_str().unwrap()]);
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(listed.contains(&sha0) && listed.contains(&sha1) && listed.lines().count() == 3, "{listed}");

    let restore = |args: &[&str], out: &Path| {
        let mut all = vec!["restore", "--db", db.to_str().unwrap(), "--out", out.to_str().unwrap()];
        all.extend(args);
        phindex(&all)
    };
    // The latest earlier content by path; any earlier content by SHA-256.
    let (latest, first) = (tmp.path().join("latest.dng"), tmp.path().join("first.dng"));
    assert!(restore(&["--path", raw.to_str().unwrap()], &latest).status.success());
    assert_eq!(fs::read(&latest).unwrap(), v1);
    assert!(restore(&["--path", raw.to_str().unwrap(), "--sha256", &sha0], &first).status.success());
    assert_eq!(fs::read(&first).unwrap(), v0);
    let again = restore(&["--path", raw.to_str().unwrap()], &latest);
    assert!(!again.status.success() && String::from_utf8_lossy(&again.stderr).contains("not overwritten"));
    assert_eq!(fs::read(&latest).unwrap(), v1);

    // The edited file moved: found by its block marks, with no path given.
    fs::create_dir_all(root.join("moved")).unwrap();
    fs::rename(&raw, root.join("moved/renamed.dng")).unwrap();
    run(&db, &root);
    let by_marks = tmp.path().join("by-marks.dng");
    let out = restore(&["--sha256", &sha0], &by_marks);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read(&by_marks).unwrap(), v0);
    // And from a file the index has never seen.
    let outside = tmp.path().join("outside.dng");
    fs::write(&outside, &v2).unwrap();
    let from = tmp.path().join("from.dng");
    assert!(restore(&["--sha256", &sha1, "--from", outside.to_str().unwrap()], &from).status.success());
    assert_eq!(fs::read(&from).unwrap(), v1);
    assert!(fs::read(&db).is_ok() && !Path::new(&format!("{}-journal", db.display())).exists(), "the index is only read");
}

#[test]
fn a_content_whose_blocks_are_gone_is_not_restored() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    let raw = root.join("one.dng");
    put(&raw, &tiff("<x:xmpmeta>rating 0</x:xmpmeta>", 1));
    run(&db, &root);
    // Not a metadata edit: the image data itself is replaced.
    replace(&raw, &tiff("<x:xmpmeta>rating 0</x:xmpmeta>", 2));
    run(&db, &root);
    assert_eq!(count(&db, "SELECT count(*) FROM versions"), 1);
    let out = tmp.path().join("out.dng");
    let r = phindex(&["restore", "--db", db.to_str().unwrap(), "--path", raw.to_str().unwrap(), "--out", out.to_str().unwrap()]);
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(!r.status.success() && err.contains("was not rebuilt") && err.contains("is not in the edited file"), "{err}");
    assert!(!out.exists() && fs::read_dir(tmp.path()).unwrap().count() == 2, "nothing is left behind");
    let none = phindex(&["restore", "--db", db.to_str().unwrap(), "--sha256", &"0".repeat(64), "--out", out.to_str().unwrap()]);
    assert!(!none.status.success() && String::from_utf8_lossy(&none.stderr).contains("no skeleton"));
}

#[test]
fn previous_revision_rows_are_upgraded_from_their_structures() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap().join("tree");
    let db = tmp.path().join("index.sqlite");
    put(&root.join("one.dng"), &tiff("<x:xmpmeta>rating 0</x:xmpmeta>", 1));
    put(&root.join("one.jpg"), &jpeg("caption"));
    put(&root.join("note.txt"), b"not an image");
    put(&root.join("clip.mov"), &vec![5u8; (1 << 20) + 1]);
    assert_eq!(run(&db, &root), 4);
    type Row = (String, Option<String>, Option<String>, String, bool);
    let snapshot = |db: &Path| -> Vec<Row> {
        let c = Connection::open(db).unwrap();
        c.prepare("SELECT path, block_marks, block_marks_err, ids_rev, sha256 IN (SELECT sha256 FROM skeletons) FROM files ORDER BY path")
            .unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap()
    };
    let want = snapshot(&db);
    let skeletons: Vec<(String, String, Vec<u8>)> = {
        let c = Connection::open(&db).unwrap();
        c.prepare("SELECT sha256, record, skeleton FROM skeletons ORDER BY sha256").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    };
    assert_eq!(skeletons.len(), 3);
    // As the build before block marks left it.
    let c = Connection::open(&db).unwrap();
    let previous = want[0].3.strip_suffix("; block-marks/1").expect("current revision").to_string();
    c.execute("UPDATE files SET ids_rev=?1, block_marks=NULL, block_marks_err=NULL", [&previous]).unwrap();
    c.execute("DELETE FROM skeletons", []).unwrap();
    drop(c);
    assert_eq!(run(&db, &root), 0, "no file is read in full");
    assert_eq!(snapshot(&db), want, "the same marks, reasons and revision as a full read gives");
    let c = Connection::open(&db).unwrap();
    let again: Vec<(String, String, Vec<u8>)> = c.prepare("SELECT sha256, record, skeleton FROM skeletons ORDER BY sha256").unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(again, skeletons, "and the same skeletons");
    // A row of an older stamp is read again in full.
    c.execute("UPDATE files SET ids_rev='an older stamp' WHERE path LIKE '%note.txt'", []).unwrap();
    drop(c);
    assert_eq!(run(&db, &root), 1);
    assert_eq!(snapshot(&db), want);
}
