//! `phindex conform` on disposable synthetic trees: a plan from an alignment,
//! renames in place with the index following, a second run, and the undo.
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A minimal RW2: IFD0 with RawDataOffset (280) only; raw data runs to the end.
fn rw2(meta: &[u8], raw: &[u8]) -> Vec<u8> {
    let mut f = b"IIU\0".to_vec();
    f.extend_from_slice(&8u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    let raw_off = 8 + 2 + 12 + 4 + meta.len();
    f.extend_from_slice(&280u16.to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&1u32.to_le_bytes());
    f.extend_from_slice(&(raw_off as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(meta);
    f.extend_from_slice(raw);
    f
}

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

fn phindex(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_phindex")).args(args).output().unwrap()
}

fn ok(args: &[&str]) -> (String, String) {
    let out = phindex(args);
    let (o, e) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
    assert!(out.status.success(), "{args:?}: {e}");
    (o, e)
}

struct Trees {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    l: PathBuf,
    r: PathBuf,
}

impl Trees {
    fn s(&self, name: &str) -> String {
        self.base.join(name).to_str().unwrap().to_string()
    }
    fn index(&self) -> String {
        ok(&["--db", &self.s("l.sqlite"), "--root", self.l.to_str().unwrap(), "--workers", "2"]);
        ok(&["--db", &self.s("r.sqlite"), "--root", self.r.to_str().unwrap(), "--workers", "2"]).1
    }
    fn align(&self, report: &str) {
        ok(&["align", "--left", &self.s("l.sqlite"), "--left-root", self.l.to_str().unwrap(), "--right", &self.s("r.sqlite"),
             "--right-root", self.r.to_str().unwrap(), "--out", &self.s(report)]);
    }
    /// (inode, SHA-256 in the right index) of a right file.
    fn seen(&self, rel: &str) -> (u64, Option<String>) {
        let p = self.r.join(rel);
        let c = rusqlite::Connection::open(self.s("r.sqlite")).unwrap();
        let sha = c.query_row("SELECT sha256 FROM files WHERE path=?1 AND gone=0", [p.to_str().unwrap()], |r| r.get(0)).ok();
        (fs::metadata(&p).unwrap_or_else(|e| panic!("{rel}: {e}")).ino(), sha)
    }
    /// The names in a right folder, as the directory spells them.
    fn names(&self, dir: &str) -> Vec<String> {
        let mut n: Vec<String> = fs::read_dir(self.r.join(dir)).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        n.sort();
        n
    }
}

fn trees() -> Trees {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (l, r) = (base.join("left"), base.join("right"));
    let raw = |n: u8| vec![n; 32];
    for n in 1..=4u8 {
        put(&l, &format!("s1/IMG_{n}.RW2"), &rw2(b"t=12:00", &raw(n)));
        put(&l, &format!("s1/IMG_{n}.JPG"), &jpeg(b"t=12:00", [n, 0x10]));
    }
    // 1: in another folder, with a sidecar the left lacks.
    put(&r, "t1/IMG_1.RW2", &rw2(b"t=12:00", &raw(1)));
    put(&r, "t1/IMG_1.JPG", &jpeg(b"t=12:00", [1, 0x10]));
    put(&r, "t1/IMG_1.xmp", b"<x:xmpmeta/>");
    // 2: under another name, and its raw's capture time shifted.
    put(&r, "s1/X_2.RW2", &rw2(b"t=12:06:42", &raw(2)));
    put(&r, "s1/X_2.JPG", &jpeg(b"t=12:00", [2, 0x10]));
    // 3: split over two folders.
    put(&r, "s1/IMG_3.RW2", &rw2(b"t=12:00", &raw(3)));
    put(&r, "other/IMG_3.JPG", &jpeg(b"t=12:00", [3, 0x10]));
    // 4: renamed, and its JPEG also copied elsewhere.
    put(&r, "s1/X_4.RW2", &rw2(b"t=12:00", &raw(4)));
    put(&r, "s1/X_4.JPG", &jpeg(b"t=12:00", [4, 0x10]));
    put(&r, "dup/IMG_4.JPG", &jpeg(b"t=12:00", [4, 0x10]));
    // 5: only the letter case of a suffix differs.
    put(&l, "s1/IMG_5.JPG", &jpeg(b"five", [5, 0x10]));
    put(&r, "s1/IMG_5.jpg", &jpeg(b"five", [5, 0x10]));
    // 6: elsewhere, and its name on the left is held by another picture on the right.
    put(&l, "s1/IMG_6.JPG", &jpeg(b"six", [6, 0x10]));
    put(&r, "a/Y_6.JPG", &jpeg(b"six", [6, 0x10]));
    put(&r, "s1/IMG_6.JPG", &jpeg(b"other", [66, 0x10]));
    // 7 and 8: two left copies of one picture the right has once.
    put(&l, "s1/IMG_7.JPG", &jpeg(b"seven", [7, 0x10]));
    put(&l, "s1/IMG_8.JPG", &jpeg(b"seven", [7, 0x10]));
    put(&r, "z/Q.JPG", &jpeg(b"seven", [7, 0x10]));
    // 9: belongs in a folder the right does not have.
    put(&l, "new/deep/IMG_9.JPG", &jpeg(b"nine", [9, 0x10]));
    put(&r, "s1/IMG_9.JPG", &jpeg(b"nine", [9, 0x10]));
    let t = Trees { _tmp: tmp, base, l, r };
    t.index();
    t.align("report");
    t
}

#[test]
fn aligned_units_are_renamed_in_place_and_back() {
    let t = trees();
    let before: Vec<(u64, Option<String>)> = ["t1/IMG_1.RW2", "t1/IMG_1.xmp", "s1/X_2.RW2", "s1/IMG_9.JPG"].iter().map(|p| t.seen(p)).collect();
    let (summary, _) = ok(&["conform", "plan", "--report", &t.s("report"), "--db", &t.s("r.sqlite"), "--out", &t.s("plan")]);
    let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
    assert_eq!((summary["units_to_rename"].as_u64(), summary["files_to_rename"].as_u64()), (Some(4), Some(7)), "{summary}");
    let skipped = fs::read_to_string(t.base.join("plan/skipped.tsv")).unwrap();
    for line in ["s1/IMG_3\ts1/IMG_3\tsplit\t1", "s1/IMG_4\ts1/X_4\tduplicates\t2", "s1/IMG_6\ta/Y_6\tdestination-taken\t1",
                 "s1/IMG_7\tz/Q\ttarget-shared\t1", "s1/IMG_8\tz/Q\ttarget-shared\t1"] {
        assert!(skipped.lines().any(|l| l == line), "{line} in {skipped}");
    }
    assert_eq!(skipped.lines().count(), 6, "{skipped}");
    // A trial plan stops before the unit that would pass the limit: units 9 and 1 have four files, unit 2 two more.
    let (trial, _) = ok(&["conform", "plan", "--report", &t.s("report"), "--db", &t.s("r.sqlite"), "--out", &t.s("trial"), "--limit", "5"]);
    let trial: serde_json::Value = serde_json::from_str(&trial).unwrap();
    assert_eq!((trial["files_to_rename"].as_u64(), trial["units_to_rename"].as_u64(), trial["stopped_at_the_limit"].as_bool()),
               (Some(4), Some(2), Some(true)), "{trial}");
    assert_eq!(fs::read_to_string(t.base.join("trial/plan.tsv")).unwrap().lines().count(), 2 + 4);
    assert_eq!(summary["stopped_at_the_limit"].as_bool(), Some(false));
    // A plan touches nothing, and is not overwritten.
    assert!(t.r.join("t1/IMG_1.RW2").exists() && !t.r.join("new").exists());
    assert!(!phindex(&["conform", "plan", "--report", &t.s("report"), "--db", &t.s("r.sqlite"), "--out", &t.s("plan")]).status.success());

    let plan = t.s("plan/plan.tsv");
    let (_, log) = ok(&["conform", "apply", "--plan", &plan, "--db", &t.s("r.sqlite")]);
    assert!(log.contains("7 files renamed in place, 0 already renamed by an earlier run, 4 units; 0 units refused"), "{log}");
    let after: Vec<(u64, Option<String>)> = ["s1/IMG_1.RW2", "s1/IMG_1.xmp", "s1/IMG_2.RW2", "new/deep/IMG_9.JPG"].iter().map(|p| t.seen(p)).collect();
    assert_eq!(before, after, "the same files, with their rows");
    assert!(after.iter().all(|(_, sha)| sha.is_some()));
    assert_eq!(t.names("s1"), ["IMG_1.JPG", "IMG_1.RW2", "IMG_1.xmp", "IMG_2.JPG", "IMG_2.RW2", "IMG_3.RW2", "IMG_5.JPG", "IMG_6.JPG",
                               "X_4.JPG", "X_4.RW2"]);
    assert!(t.names("t1").is_empty() && t.r.join("a/Y_6.JPG").exists() && t.r.join("z/Q.JPG").exists());
    // The index followed: the next run reads nothing and finds nothing moved.
    let log = t.index();
    assert!(log.contains(", 0 to read") && log.contains(" 0 moved or renamed"), "{log}");
    // The alignment now finds these units under their left names.
    t.align("report2");
    let units = fs::read_to_string(t.base.join("report2/units.tsv")).unwrap();
    for (unit, status) in [("s1/IMG_1", "identical"), ("s1/IMG_2", "metadata-differs"), ("s1/IMG_5", "identical"), ("new/deep/IMG_9", "identical")] {
        let u: Vec<&str> = units.lines().find(|l| l.starts_with(&format!("{unit}\t"))).unwrap().split('\t').collect();
        assert_eq!((u[2], u[3], u[4]), (status, if unit == "s1/IMG_1" { "right-has-more" } else { "" }, unit), "{u:?}");
    }
    // A second apply finds every rename made.
    let (_, log) = ok(&["conform", "apply", "--plan", &plan, "--db", &t.s("r.sqlite")]);
    assert!(log.contains("0 files renamed in place, 7 already renamed by an earlier run"), "{log}");

    let journal = format!("{plan}.journal");
    let (_, log) = ok(&["conform", "undo", "--journal", &journal, "--db", &t.s("r.sqlite")]);
    assert!(log.contains("7 files renamed back, 0 were already under their old names, 2 empty folders removed; 0 refused"), "{log}");
    let back: Vec<(u64, Option<String>)> = ["t1/IMG_1.RW2", "t1/IMG_1.xmp", "s1/X_2.RW2", "s1/IMG_9.JPG"].iter().map(|p| t.seen(p)).collect();
    assert_eq!(before, back);
    assert!(!t.r.join("new").exists());
    assert_eq!(t.names("s1"), ["IMG_3.RW2", "IMG_5.jpg", "IMG_6.JPG", "IMG_9.JPG", "X_2.JPG", "X_2.RW2", "X_4.JPG", "X_4.RW2"]);
    let log = t.index();
    assert!(log.contains(", 0 to read") && log.contains(" 0 moved or renamed"), "{log}");
    let (_, log) = ok(&["conform", "undo", "--journal", &journal, "--db", &t.s("r.sqlite")]);
    assert!(log.contains("0 files renamed back, 7 were already under their old names"), "{log}");
}

#[test]
fn a_unit_with_a_changed_file_or_a_taken_name_is_left_whole() {
    let t = trees();
    ok(&["conform", "plan", "--report", &t.s("report"), "--db", &t.s("r.sqlite"), "--out", &t.s("plan")]);
    // After the plan: one file of unit 2 is edited, and unit 9's name is taken.
    put(&t.r, "s1/X_2.JPG", &jpeg(b"edited since", [2, 0x10]));
    put(&t.r, "new/deep/IMG_9.JPG", &jpeg(b"someone else", [99, 0x10]));
    let out = phindex(&["conform", "apply", "--plan", &t.s("plan/plan.tsv"), "--db", &t.s("r.sqlite")]);
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{log}");
    assert!(log.contains("4 files renamed in place") && log.contains("2 units refused (3 files)"), "{log}");
    assert!(log.contains("X_2.JPG has changed since it was listed") && log.contains("new/deep/IMG_9.JPG exists"), "{log}");
    // Unit 2 keeps both names; unit 9 and the file in its place are untouched.
    assert!(t.r.join("s1/X_2.RW2").exists() && t.r.join("s1/X_2.JPG").exists() && !t.r.join("s1/IMG_2.RW2").exists());
    assert_eq!(fs::read(t.r.join("new/deep/IMG_9.JPG")).unwrap(), jpeg(b"someone else", [99, 0x10]));
    assert!(t.r.join("s1/IMG_9.JPG").exists());
    // The undo takes back what was renamed and leaves the folder it did not create.
    let (_, log) = ok(&["conform", "undo", "--journal", &t.s("plan/plan.tsv.journal"), "--db", &t.s("r.sqlite")]);
    assert!(log.contains("4 files renamed back") && log.contains("0 empty folders removed"), "{log}");
    assert!(t.r.join("t1/IMG_1.xmp").exists() && t.r.join("s1/IMG_5.jpg").exists() && t.r.join("new/deep").exists());
}
