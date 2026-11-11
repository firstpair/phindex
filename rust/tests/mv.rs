//! `phindex diff` and `phindex mv` on disposable synthetic trees: the files of
//! one tree not aligned with another, listed, moved aside with the index
//! following, and moved back.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn phindex(args: &[&str]) -> (Output, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_phindex")).args(args).output().unwrap();
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    (out, log)
}

fn ok(args: &[&str]) -> (String, String) {
    let (out, log) = phindex(args);
    assert!(out.status.success(), "{args:?}: {log}");
    (String::from_utf8_lossy(&out.stdout).into_owned(), log)
}

struct Trees {
    _tmp: tempfile::TempDir,
    base: PathBuf,
}

impl Trees {
    fn s(&self, name: &str) -> String {
        self.base.join(name).to_str().unwrap().to_string()
    }
    /// An indexing run of the tree `b`'s index over ROOT; its log.
    fn index(&self, root: &str) -> String {
        ok(&["--db", &self.s("b.sqlite"), "--root", &self.s(root), "--workers", "2"]).1
    }
    /// Whether the index has a live, read row at this path.
    fn row(&self, rel: &str) -> bool {
        let c = rusqlite::Connection::open(self.s("b.sqlite")).unwrap();
        c.query_row("SELECT count(*) FROM files WHERE path=?1 AND gone=0 AND sha256 IS NOT NULL", [self.s(rel)], |r| r.get::<_, i64>(0)).unwrap() == 1
    }
    fn at(&self, rel: &str) -> bool {
        self.base.join(rel).is_file() && self.row(rel)
    }
}

fn trees() -> Trees {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (b, p) = (base.join("b"), base.join("p"));
    for root in [&b, &p] {
        put(root, "s/A.JPG", &jpeg(b"a", [1, 0x10]));
    }
    // Same path, metadata edited; same path, a sidecar rewritten.
    put(&b, "s/B.JPG", &jpeg(b"b, later", [2, 0x10]));
    put(&p, "s/B.JPG", &jpeg(b"b", [2, 0x10]));
    put(&b, "s/C.xmp", b"<x:xmpmeta>mine</x:xmpmeta>");
    put(&p, "s/C.xmp", b"<x:xmpmeta/>");
    // Paths the other tree lacks: a copy, a variant, and two files only here.
    put(&b, "s/D.JPG", &jpeg(b"d", [4, 0x10]));
    put(&p, "other/D2.JPG", &jpeg(b"d", [4, 0x10]));
    put(&b, "s/E.JPG", &jpeg(b"e, edited", [5, 0x10]));
    put(&p, "s/E2.JPG", &jpeg(b"e", [5, 0x10]));
    put(&b, "x/y/F.JPG", &jpeg(b"f", [6, 0x10]));
    put(&b, "x/y/.hidden", b"only here");
    let t = Trees { _tmp: tmp, base };
    t.index("b");
    ok(&["--db", &t.s("p.sqlite"), "--root", &t.s("p"), "--workers", "2"]);
    t
}

fn diff(t: &Trees, out: &str, more: &[&str]) -> serde_json::Value {
    let (b, p, bdb, pdb, out) = (t.s("b"), t.s("p"), t.s("b.sqlite"), t.s("p.sqlite"), t.s(out));
    let mut args = vec!["diff", "--left", &bdb, "--left-root", &b, "--right", &pdb, "--right-root", &p, "--out", &out];
    args.extend(more);
    serde_json::from_str(&ok(&args).0).unwrap()
}

#[test]
fn files_not_aligned_are_listed_moved_aside_and_moved_back() {
    let t = trees();
    let d = diff(&t, "diff", &[]);
    let files = |state: &str| d["left_files_by_state"][state]["files"].as_u64();
    assert_eq!((files("same"), files("same-image"), files("changed"), files("copy"), files("variant"), files("only-left")),
               (Some(1), Some(1), Some(1), Some(1), Some(1), Some(2)), "{d}");
    let list = fs::read_to_string(t.base.join("diff/diff.tsv")).unwrap();
    let listed: Vec<(&str, &str)> = list.lines().skip(2).map(|l| { let f: Vec<&str> = l.split('\t').collect(); (f[0], f[1]) }).collect();
    assert_eq!(listed, [("s/D.JPG", "copy"), ("s/E.JPG", "variant"), ("x/y/.hidden", "only-left"), ("x/y/F.JPG", "only-left")]);
    assert_eq!(diff(&t, "diff-changed", &["--states", "changed"])["files_listed"].as_u64(), Some(1));

    // Without --files only the index would move, and no file is at its new path yet.
    let list = t.s("diff/diff.tsv");
    let (out, log) = phindex(&["mv", "--db", &t.s("b.sqlite"), "--list", &list, "--to", &t.s("review")]);
    assert!(!out.status.success() && log.contains("0 index rows moved, no file touched; 4 refused"), "{log}");
    assert!(t.at("b/s/D.JPG") && !t.base.join("review").exists());

    // A trial of two, then the rest; the emptied folders go.
    let (_, log) = ok(&["mv", "--db", &t.s("b.sqlite"), "--files", "--list", &list, "--to", &t.s("review"), "--limit", "2"]);
    assert!(log.contains("2 files renamed in place, 0 already renamed"), "{log}");
    let (_, log) = ok(&["mv", "--db", &t.s("b.sqlite"), "--files", "--list", &list, "--to", &t.s("review"), "--prune-empty"]);
    assert!(log.contains("2 files renamed in place, 2 already renamed by an earlier run, 4 units; 0 units refused") &&
            log.contains("2 emptied folders removed"), "{log}");
    for rel in ["s/D.JPG", "s/E.JPG", "x/y/.hidden", "x/y/F.JPG"] {
        assert!(t.at(&format!("review/{rel}")) && !t.base.join("b").join(rel).exists() && !t.row(&format!("b/{rel}")), "{rel}");
    }
    assert!(!t.base.join("b/x").exists() && t.at("b/s/A.JPG") && t.at("b/s/C.xmp"));
    // The index followed in both places, and the tree is now a subset by path.
    for root in ["b", "review"] {
        let log = t.index(root);
        assert!(log.contains(", 0 to read") && log.contains(" 0 moved or renamed"), "{root}: {log}");
    }
    assert_eq!(diff(&t, "diff2", &[])["files_listed"].as_u64(), Some(0));

    let (_, log) = ok(&["mv", "undo", "--journal", &format!("{list}.journal"), "--db", &t.s("b.sqlite")]);
    assert!(log.contains("4 files renamed back") && log.contains("0 refused"), "{log}");
    for rel in ["s/D.JPG", "s/E.JPG", "x/y/.hidden", "x/y/F.JPG"] {
        assert!(t.at(&format!("b/{rel}")) && !t.base.join("review").join(rel).exists(), "{rel}");
    }
    assert!(!t.base.join("review").exists());
}

#[test]
fn one_file_or_a_folder_moves_with_its_rows() {
    let t = trees();
    let db = t.s("b.sqlite");
    // A file moved by other means: the index is told, after a check that it is there.
    let (out, log) = phindex(&["mv", "--db", &db, &t.s("b/s/D.JPG"), &t.s("aside/D.JPG")]);
    assert!(!out.status.success() && log.contains("is still there"), "{log}");
    fs::create_dir_all(t.base.join("aside")).unwrap();
    fs::rename(t.base.join("b/s/D.JPG"), t.base.join("aside/D.JPG")).unwrap();
    let (_, log) = ok(&["mv", "--db", &db, &t.s("b/s/D.JPG"), &t.s("aside/D.JPG")]);
    assert!(log.contains("1 index row moved; no file was touched") && t.at("aside/D.JPG") && !t.row("b/s/D.JPG"), "{log}");
    // --force moves a row whatever is on disk.
    let (_, log) = ok(&["mv", "--db", &db, "--force", &t.s("b/s/E.JPG"), &t.s("aside/E.JPG")]);
    assert!(log.contains("no file was touched") && t.row("aside/E.JPG") && t.base.join("b/s/E.JPG").exists(), "{log}");
    ok(&["mv", "--db", &db, "--force", &t.s("aside/E.JPG"), &t.s("b/s/E.JPG")]);
    // --files moves the file too.
    let (_, log) = ok(&["mv", "--db", &db, "--files", &t.s("b/s/E.JPG"), &t.s("aside/deep/E.JPG")]);
    assert!(log.contains("the file renamed") && t.at("aside/deep/E.JPG") && !t.base.join("b/s/E.JPG").exists(), "{log}");

    // A folder: its whole subtree, by one rename, never merged into an existing folder.
    let (out, log) = phindex(&["mv", "--db", &db, "--files", &t.s("b/x"), &t.s("aside")]);
    assert!(!out.status.success() && log.contains("the index lists files under"), "{log}");
    fs::create_dir_all(t.base.join("empty")).unwrap();
    let (out, log) = phindex(&["mv", "--db", &db, "--files", &t.s("b/x"), &t.s("empty")]);
    assert!(!out.status.success() && log.contains("exists") && t.base.join("b/x/y/F.JPG").exists(), "{log}");
    let (_, log) = ok(&["mv", "--db", &db, "--files", &t.s("b/x"), &t.s("aside/x")]);
    assert!(log.contains("2 index rows moved") && log.contains("the folder renamed"), "{log}");
    assert!(t.at("aside/x/y/F.JPG") && t.at("aside/x/y/.hidden") && !t.base.join("b/x").exists());
    let log = t.index("aside");
    assert!(log.contains(", 0 to read") && log.contains(" 0 moved or renamed"), "{log}");
    // A folder moved by other means: rows only, after the same check.
    let (out, log) = phindex(&["mv", "--db", &db, &t.s("b/s"), &t.s("b/t")]);
    assert!(!out.status.success() && log.contains("3 of 3 files are not under"), "{log}");
    fs::rename(t.base.join("b/s"), t.base.join("b/t")).unwrap();
    let (_, log) = ok(&["mv", "--db", &db, &t.s("b/s"), &t.s("b/t")]);
    assert!(log.contains("3 index rows moved") && log.contains("no file was touched") && t.at("b/t/A.JPG"), "{log}");
    let log = t.index("b");
    assert!(log.contains(", 0 to read") && log.contains(" 0 moved or renamed"), "{log}");
}
