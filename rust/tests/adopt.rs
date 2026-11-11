//! `phindex adopt` on disposable synthetic trees: files copied from one
//! indexed tree into another get their rows from the first tree's index.
use std::fs;
use std::path::Path;
use std::process::Command;

/// A JPEG skeleton: APP1 (metadata), DQT, SOF0, SOS and two bytes of scan.
fn jpeg(app1: &[u8], scan: [u8; 2]) -> Vec<u8> {
    let mut f = vec![0xFF, 0xD8, 0xFF, 0xE1];
    f.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    f.extend_from_slice(app1);
    f.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x03, 0x00, 0xFF, 0xC0, 0x00, 0x03, 0x08, 0xFF, 0xDA, 0x00, 0x03, 0x00]);
    f.extend_from_slice(&scan);
    f.extend_from_slice(&[0xFF, 0xD9]);
    f
}

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

fn phindex(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_phindex")).args(args).output().unwrap();
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{args:?}: {log}");
    log
}

/// A copy that keeps the modification time, as a synchronizer makes it.
fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    assert!(Command::new("cp").arg("-p").arg(from).arg(to).status().unwrap().success());
}

#[test]
fn copies_take_their_rows_from_the_other_index() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (p, b) = (base.join("p"), base.join("b"));
    let s = |name: &str| base.join(name).to_str().unwrap().to_string();
    put(&p, "s/A.JPG", &jpeg(b"a", [1, 0x10]));
    put(&p, "s/B.JPG", &jpeg(b"b, edited", [2, 0x10]));
    put(&p, "s/C.JPG", &jpeg(b"c", [3, 0x10]));
    put(&p, "deep/er/D.xmp", b"<x:xmpmeta/>");
    put(&p, "s/E.JPG", &jpeg(b"e", [5, 0x10]));
    // The receiving tree starts with A as it is, an older B, and a file of its own.
    copy(&p.join("s/A.JPG"), &b.join("s/A.JPG"));
    put(&b, "s/B.JPG", &jpeg(b"b", [2, 0x10]));
    put(&b, "s/own.JPG", &jpeg(b"own", [9, 0x10]));
    phindex(&["--db", &s("p.sqlite"), "--root", &s("p"), "--workers", "2"]);
    phindex(&["--db", &s("b.sqlite"), "--root", &s("b"), "--workers", "2"]);
    let adopt = || phindex(&["adopt", "--db", &s("b.sqlite"), "--root", &s("b"), "--from", &s("p.sqlite"), "--from-root", &s("p")]);
    let log = adopt();
    assert!(log.contains("0 rows adopted") && log.contains("1 already current; 3 not there; 1 there with another size or time"), "{log}");

    // A synchronization: B replaced, C and D new, with times kept; E copied without its time.
    for rel in ["s/B.JPG", "s/C.JPG", "deep/er/D.xmp"] {
        copy(&p.join(rel), &b.join(rel));
    }
    fs::copy(p.join("s/E.JPG"), b.join("s/E.JPG")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(b.join("s/E.JPG"), jpeg(b"e", [5, 0x10])).unwrap();
    let log = adopt();
    assert!(log.contains("3 rows adopted without opening a file") && log.contains("1 already current; 0 not there; 1 there with another size or time"), "{log}");

    // The adopted rows are the other index's, under this tree's paths and inodes.
    let (pc, bc) = (rusqlite::Connection::open(s("p.sqlite")).unwrap(), rusqlite::Connection::open(s("b.sqlite")).unwrap());
    let ids = "sha256, image_data, md_digest, block_marks, block_marks_err, ids_rev, size";
    let row = |c: &rusqlite::Connection, path: String| -> Vec<Option<String>> {
        c.query_row(&format!("SELECT {ids} FROM files WHERE path=?1 AND gone=0"), [path], |r| (0..7).map(|i| r.get::<_, rusqlite::types::Value>(i).map(|v| match v {
            rusqlite::types::Value::Text(t) => Some(t),
            rusqlite::types::Value::Integer(n) => Some(n.to_string()),
            _ => None,
        })).collect()).unwrap()
    };
    for rel in ["s/B.JPG", "s/C.JPG", "deep/er/D.xmp"] {
        let (theirs, ours) = (row(&pc, s(&format!("p/{rel}"))), row(&bc, s(&format!("b/{rel}"))));
        assert!(theirs[0].is_some() && theirs == ours, "{rel}: {theirs:?} {ours:?}");
        let ino: i64 = bc.query_row("SELECT ino FROM files WHERE path=?1", [s(&format!("b/{rel}"))], |r| r.get(0)).unwrap();
        assert_eq!(ino as u64, std::os::unix::fs::MetadataExt::ino(&fs::metadata(b.join(rel)).unwrap()));
    }
    let skeletons: i64 = bc.query_row("SELECT count(*) FROM skeletons s JOIN files f ON f.sha256 = s.sha256 WHERE f.path IN (?1, ?2)",
                                      [s("b/s/B.JPG"), s("b/s/C.JPG")], |r| r.get(0)).unwrap();
    assert_eq!(skeletons, 2);
    // The record names this tree's folder.
    let packed: Vec<u8> = bc.query_row("SELECT record_deflate FROM metadata WHERE path=?1", [s("b/s/C.JPG")], |r| r.get(0)).unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut flate2::read::DeflateDecoder::new(&packed[..]), &mut text).unwrap();
    assert!(text.contains(&format!("\"File:System:Directory\":\"{}\"", s("b/s"))), "{text}");
    // B's earlier content is a version, and is restored from the file as it is now.
    let versions: i64 = bc.query_row("SELECT count(*) FROM versions WHERE path=?1", [s("b/s/B.JPG")], |r| r.get(0)).unwrap();
    assert_eq!(versions, 1);
    drop((pc, bc));
    phindex(&["restore", "--db", &s("b.sqlite"), "--path", &s("b/s/B.JPG"), "--out", &s("B.before")]);
    assert_eq!(fs::read(base.join("B.before")).unwrap(), jpeg(b"b", [2, 0x10]));

    // An ordinary run then reads only the file whose time was not kept.
    let log = phindex(&["--db", &s("b.sqlite"), "--root", &s("b"), "--workers", "2"]);
    assert!(log.contains(", 1 to read") && log.contains(" 0 moved or renamed"), "{log}");
    assert!(adopt().contains("0 rows adopted"));
}
