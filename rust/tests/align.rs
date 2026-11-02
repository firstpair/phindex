//! `phindex align` on disposable synthetic trees of RW2+JPEG units: moves,
//! renames, metadata edits, a re-rendered JPEG, a lone JPEG and right-only files.
use std::fs;
use std::path::Path;
use std::process::Command;

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

fn index(db: &Path, root: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_phindex"))
        .args(["--db", db.to_str().unwrap(), "--root", root.to_str().unwrap(), "--workers", "2"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn units_align_by_identity_and_follow_their_raw() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let (l, r) = (base.join("left"), base.join("right"));
    let raw = |n: u8| vec![n; 32];
    for n in 1..=5u8 {
        put(&l, &format!("s1/IMG_{n}.RW2"), &rw2(b"t=12:00", &raw(n)));
        put(&l, &format!("s1/IMG_{n}.JPG"), &jpeg(b"t=12:00", [n, 0x10]));
    }
    put(&l, "s1/IMG_6.JPG", &jpeg(b"alone", [6, 0x10]));
    // 1: moved to another folder unchanged.
    put(&r, "t1/IMG_1.RW2", &rw2(b"t=12:00", &raw(1)));
    put(&r, "t1/IMG_1.JPG", &jpeg(b"t=12:00", [1, 0x10]));
    // 2: capture time shifted in both files.
    put(&r, "s1/IMG_2.RW2", &rw2(b"t=12:06:42", &raw(2)));
    put(&r, "s1/IMG_2.JPG", &jpeg(b"t=12:06:42", [2, 0x10]));
    // 3: only the raw survives; a copy of its JPEG lives elsewhere too.
    put(&r, "s1/IMG_3.RW2", &rw2(b"t=12:00", &raw(3)));
    put(&r, "other/IMG_3.JPG", &jpeg(b"t=12:00", [3, 0x10]));
    // 4: renamed, unchanged; its JPEG also duplicated in another folder.
    put(&r, "s1/X_4.RW2", &rw2(b"t=12:00", &raw(4)));
    put(&r, "s1/X_4.JPG", &jpeg(b"t=12:00", [4, 0x10]));
    put(&r, "dup/IMG_4.JPG", &jpeg(b"t=12:00", [4, 0x10]));
    // 5: raw unchanged, JPEG re-rendered (different image data).
    put(&r, "s1/IMG_5.RW2", &rw2(b"t=12:00", &raw(5)));
    put(&r, "s1/IMG_5.JPG", &jpeg(b"t=12:00", [5, 0x77]));
    // Right only.
    put(&r, "s1/IMG_9.JPG", &jpeg(b"new", [9, 0x10]));
    let (ldb, rdb) = (base.join("l.sqlite"), base.join("r.sqlite"));
    index(&ldb, &l);
    index(&rdb, &r);
    // 7: a left file not yet read is reported as not indexed.
    put(&l, "s1/IMG_7.JPG", &jpeg(b"late", [7, 0x10]));
    let c = rusqlite::Connection::open(&ldb).unwrap();
    c.execute("INSERT INTO files (path,size,mtime_ns,ino,dev,seen_unix) VALUES (?1,1,1,1,1,1)",
              [l.join("s1/IMG_7.JPG").to_str().unwrap()]).unwrap();
    drop(c);

    let report = base.join("report");
    let out = Command::new(env!("CARGO_BIN_EXE_phindex"))
        .args(["align", "--left", ldb.to_str().unwrap(), "--left-root", l.to_str().unwrap(), "--right", rdb.to_str().unwrap(),
               "--right-root", r.to_str().unwrap(), "--out", report.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let units = fs::read_to_string(report.join("units.tsv")).unwrap();
    let unit = |name: &str| -> Vec<String> {
        units.lines().find(|l| l.starts_with(&format!("s1/{name}\t"))).unwrap_or_else(|| panic!("{name} in {units}"))
            .split('\t').map(String::from).collect()
    };
    let check = |name: &str, kind: &str, status: &str, flags: &str, right: &str| {
        let u = unit(name);
        assert_eq!((u[1].as_str(), u[2].as_str(), u[3].as_str(), u[4].as_str()), (kind, status, flags, right), "{name}: {u:?}");
    };
    check("IMG_1", "raw+jpeg", "identical", "moved", "t1/IMG_1");
    check("IMG_2", "raw+jpeg", "metadata-differs", "", "s1/IMG_2");
    check("IMG_3", "raw+jpeg", "identical", "split", "s1/IMG_3");
    check("IMG_4", "raw+jpeg", "identical", "renamed,duplicates", "s1/X_4");
    check("IMG_5", "raw+jpeg", "image-differs", "", "s1/IMG_5");
    check("IMG_6", "jpeg", "left-only", "", "");
    check("IMG_7", "jpeg", "not-indexed", "", "");
    assert!(unit("IMG_2")[5].contains("RW2:image-metadata") && unit("IMG_2")[5].contains("JPG:image-metadata"));
    let pairs = fs::read_to_string(report.join("image-pairs.tsv")).unwrap();
    assert_eq!(pairs.lines().count(), 3, "IMG_2's two files and IMG_5's JPEG: {pairs}");
    let right_only = fs::read_to_string(report.join("right-only.tsv")).unwrap();
    assert!(right_only.contains("s1/IMG_9\tjpeg\tnew"), "{right_only}");
    assert!(right_only.contains("dup/IMG_4\tjpeg\tcopy"), "a second copy of left content is not new: {right_only}");
    assert!(!right_only.contains("other/IMG_3"), "IMG_3's JPEG is matched there (split): {right_only}");
}
