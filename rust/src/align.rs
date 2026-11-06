//! `phindex align`: account for every file under a left root in a right root,
//! by identity, not by name.
//!
//!   phindex align --left A.sqlite --left-root DIR --right B.sqlite --right-root DIR --out REPORT_DIR
//!
//! Files are grouped into units: the files of one folder with the same name up to
//! the first dot (IMG_1.DNG, IMG_1.JPG, IMG_1.xmp). A unit's kind names its
//! images: `dng`, `dng+jpeg`, `raw`, `raw+jpeg`, `jpeg` and so on; sidecars ride
//! along. Each left file is matched by its strongest shared identity:
//!
//! | tier | meaning |
//! |---|---|
//! | `identical` | equal SHA-256 |
//! | `xmp-only` | DNG raw blocks, DNG profile and preserved digest equal: only the XMP packet differs |
//! | `dng-metadata` | DNG raw blocks equal, other bytes differ |
//! | `image-metadata` | image data equal (JPEG, TIFF, non-DNG raw), other bytes differ |
//! | `lightroom` | only Lightroom's original fingerprint equal |
//! | `unit-partner` | no identity shared, but the file's unit partner on the right, by kind |
//!
//! A unit is `image-differs` only when an image member (DNG, raw, JPEG, other
//! image) is a unit partner; a changed sidecar or settings file makes it
//! `metadata-differs`.
//!
//! Units are matched as a whole: the unit's DNG or raw is matched first, and its
//! other members prefer candidates in the same right unit, so a JPEG duplicated
//! elsewhere follows its raw. A member that shares no identity takes the right
//! unit's member of the same kind as `unit-partner` (say, a re-rendered JPEG).
//! Rows not yet read are reported as not indexed, never as missing; while any
//! right row is unread, an unmatched left unit is `not-found-yet`, not `left-only`.
//!
//! Report files: summary.json; units.tsv (one line per left unit); files.tsv
//! (one line per left file); right-only.tsv (right units no left file maps to,
//! as `new`, or `copy` when every member's bytes are also on the left);
//! dng-pairs.tsv and image-pairs.tsv (absolute LEFT<TAB>RIGHT paths of files
//! matched by a shared identity whose bytes differ, for DNG Compare's --pairs and
//! metadata diffs); partner-pairs.tsv (KIND<TAB>LEFT<TAB>RIGHT for image files
//! matched only as unit partners: they share no identity, so a pixel comparison
//! would only confirm they differ, or fail on a damaged file).
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, params};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Kind {
    Dng,
    Raw,
    Jpeg,
    Image,
    Sidecar,
    Other,
}

fn kind_of(name: &str) -> Kind {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "dng" => Kind::Dng,
        "nef" | "nrw" | "cr2" | "cr3" | "crw" | "arw" | "srf" | "sr2" | "rw2" | "rwl" | "raf" | "orf" | "pef" | "3fr" | "fff"
        | "iiq" | "erf" | "mef" | "mos" | "dcr" | "kdc" | "srw" | "x3f" | "gpr" => Kind::Raw,
        "jpg" | "jpeg" => Kind::Jpeg,
        "heic" | "heif" | "tif" | "tiff" | "png" | "psd" => Kind::Image,
        "xmp" | "jpg_xmp" | "dng_xmp" => Kind::Sidecar,
        _ => Kind::Other,
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Dng => "dng",
        Kind::Raw => "raw",
        Kind::Jpeg => "jpeg",
        Kind::Image => "image",
        Kind::Sidecar => "sidecar",
        Kind::Other => "other",
    }
}

fn is_image(k: Kind) -> bool {
    matches!(k, Kind::Dng | Kind::Raw | Kind::Jpeg | Kind::Image)
}

/// The unit's image kinds in a fixed order ("raw+jpeg"); "sidecar" or "other"
/// when it has no image.
fn unit_kind(side: &Side, members: &[usize]) -> String {
    let mut k: Vec<Kind> = members.iter().map(|&i| side.rows[i].kind).filter(|&k| is_image(k)).collect();
    if k.is_empty() {
        k = members.iter().map(|&i| side.rows[i].kind).collect();
        k.sort();
        k.truncate(1);
    }
    k.sort();
    k.dedup();
    k.iter().map(|&k| kind_name(k)).collect::<Vec<_>>().join("+")
}

struct Row {
    rel: String,
    unit: usize,
    kind: Kind,
    sha: Option<String>,
    blocks: Option<String>,
    preserved: Option<(String, String)>,
    image: Option<String>,
    lr: Option<String>,
}

struct Side {
    root: String,
    rows: Vec<Row>,
    /// (folder, stem) of each unit, and its member rows.
    units: Vec<((String, String), Vec<usize>)>,
}

/// (folder relative to the root, name up to the first dot).
fn unit_key(rel: &str) -> (String, String) {
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let stem = name.split_once('.').map(|(s, _)| s).unwrap_or(name);
    (dir.to_string(), stem.to_string())
}

fn load(db: &str, root: &str) -> Result<Side> {
    let root = root.trim_end_matches('/').to_string();
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut st = c.prepare("SELECT path, sha256, dng_blocks, preserved, dngsem_profile, image_data, lr_digest
                            FROM files WHERE gone=0 AND path >= ?1 AND path < ?2 ORDER BY path")?;
    let mut rows = Vec::new();
    let mut unit_ids: HashMap<(String, String), usize> = HashMap::new();
    let mut units: Vec<((String, String), Vec<usize>)> = Vec::new();
    let mut q = st.query(params![format!("{root}/"), format!("{root}0")])?;
    while let Some(r) = q.next()? {
        let path: String = r.get(0)?;
        let rel = path[root.len() + 1..].to_string();
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        if name.starts_with('.') {
            continue; // .DS_Store, AppleDouble files
        }
        let key = unit_key(&rel);
        let unit = *unit_ids.entry(key.clone()).or_insert_with(|| {
            units.push((key, Vec::new()));
            units.len() - 1
        });
        units[unit].1.push(rows.len());
        let preserved = match (r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?) {
            (Some(p), Some(prof)) => Some((p, prof)),
            _ => None,
        };
        rows.push(Row { kind: kind_of(name), rel, unit, sha: r.get(1)?, blocks: r.get(2)?, preserved, image: r.get(5)?, lr: r.get(6)? });
    }
    Ok(Side { root, rows, units })
}

fn by<'a>(rows: &'a [Row], f: impl Fn(&'a Row) -> Option<&'a str>) -> HashMap<&'a str, Vec<usize>> {
    let mut m: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(v) = f(r) {
            m.entry(v).or_default().push(i);
        }
    }
    m
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Tier {
    Identical,
    XmpOnly,
    DngMetadata,
    ImageMetadata,
    Lightroom,
    UnitPartner,
}

fn tier_name(t: Tier) -> &'static str {
    match t {
        Tier::Identical => "identical",
        Tier::XmpOnly => "xmp-only",
        Tier::DngMetadata => "dng-metadata",
        Tier::ImageMetadata => "image-metadata",
        Tier::Lightroom => "lightroom",
        Tier::UnitPartner => "unit-partner",
    }
}

struct Match {
    right: usize,
    tier: Tier,
    candidates: usize,
}

struct Indexes<'a> {
    sha: HashMap<&'a str, Vec<usize>>,
    blocks: HashMap<&'a str, Vec<usize>>,
    image: HashMap<&'a str, Vec<usize>>,
    lr: HashMap<&'a str, Vec<usize>>,
}

/// The strongest tier with candidates, and its candidates.
fn candidates(l: &Row, ix: &Indexes, right: &[Row]) -> Option<(Tier, Vec<usize>)> {
    if let Some(v) = l.sha.as_deref().and_then(|s| ix.sha.get(s)) {
        return Some((Tier::Identical, v.clone()));
    }
    if let Some(v) = l.blocks.as_deref().and_then(|s| ix.blocks.get(s)) {
        let xmp_only: Vec<usize> = v.iter().copied().filter(|&i| l.preserved.is_some() && right[i].preserved == l.preserved).collect();
        return Some(if xmp_only.is_empty() { (Tier::DngMetadata, v.clone()) } else { (Tier::XmpOnly, xmp_only) });
    }
    if let Some(v) = l.image.as_deref().and_then(|s| ix.image.get(s)) {
        return Some((Tier::ImageMetadata, v.clone()));
    }
    if let Some(v) = l.lr.as_deref().and_then(|s| ix.lr.get(s)) {
        return Some((Tier::Lightroom, v.clone()));
    }
    None
}

/// Prefer the same relative path, then the right unit already chosen for this
/// left unit, then the same file name, then the first path.
fn choose(l: &Row, cands: &[usize], right: &Side, target: Option<usize>) -> usize {
    let name = |rel: &str| rel.rsplit('/').next().unwrap_or(rel).to_string();
    *cands.iter().min_by_key(|&&i| {
        let r = &right.rows[i];
        (r.rel != l.rel, Some(r.unit) != target, name(&r.rel) != name(&l.rel), r.rel.clone())
    }).unwrap()
}

pub fn run(args: &[String]) -> Result<()> {
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let need = |flag: &str| get(flag).with_context(|| format!("{flag} required"));
    let (left, right) = (load(&need("--left")?, &need("--left-root")?)?, load(&need("--right")?, &need("--right-root")?)?);
    let out = need("--out")?;
    if left.rows.is_empty() {
        bail!("no files under {} in the left index", left.root);
    }
    fs::create_dir_all(&out)?;
    let ix = Indexes {
        sha: by(&right.rows, |r| r.sha.as_deref()),
        blocks: by(&right.rows, |r| r.blocks.as_deref()),
        image: by(&right.rows, |r| r.image.as_deref()),
        lr: by(&right.rows, |r| r.lr.as_deref()),
    };

    // While right rows are unread, an unmatched left unit may still be found.
    let right_complete = right.rows.iter().all(|r| r.sha.is_some());
    let mut matches: Vec<Option<Match>> = (0..left.rows.len()).map(|_| None).collect();
    let mut used_right_units: HashSet<usize> = HashSet::new();
    let mut unit_target: Vec<Option<usize>> = vec![None; left.units.len()];
    for (u, (_, members)) in left.units.iter().enumerate() {
        // Images that identify the unit first: DNG, other raw, image, JPEG; then the rest.
        let mut order = members.clone();
        order.sort_by_key(|&i| left.rows[i].kind);
        let mut target: Option<usize> = None;
        for &i in &order {
            let l = &left.rows[i];
            if let Some((tier, cands)) = candidates(l, &ix, &right.rows) {
                let pick = choose(l, &cands, &right, target);
                target = target.or(Some(right.rows[pick].unit));
                matches[i] = Some(Match { right: pick, tier, candidates: cands.len() });
            }
        }
        // Members sharing no identity take the target unit's member of the same kind.
        if let Some(t) = target {
            let taken: HashSet<usize> = order.iter().filter_map(|&i| matches[i].as_ref().map(|m| m.right)).collect();
            for &i in &order {
                if matches[i].is_some() || left.rows[i].sha.is_none() {
                    continue;
                }
                let partner = right.units[t].1.iter().copied()
                    .find(|&j| right.rows[j].kind == left.rows[i].kind && !taken.contains(&j));
                if let Some(j) = partner {
                    matches[i] = Some(Match { right: j, tier: Tier::UnitPartner, candidates: 1 });
                }
            }
        }
        for &i in members {
            if let Some(m) = &matches[i] {
                used_right_units.insert(right.rows[m.right].unit);
            }
        }
        unit_target[u] = target;
    }

    let w = |name: &str| -> Result<BufWriter<fs::File>> { Ok(BufWriter::new(fs::File::create(Path::new(&out).join(name))?)) };
    let (mut units_tsv, mut files_tsv, mut dng_pairs, mut image_pairs, mut right_only) =
        (w("units.tsv")?, w("files.tsv")?, w("dng-pairs.tsv")?, w("image-pairs.tsv")?, w("right-only.tsv")?);
    let mut partner_pairs = w("partner-pairs.tsv")?;
    writeln!(units_tsv, "left_unit\tkind\tstatus\tflags\tright_unit\tmembers")?;
    writeln!(files_tsv, "left\tright\ttier\tcandidates")?;
    let mut by_kind: BTreeMap<String, BTreeMap<&'static str, u64>> = BTreeMap::new();
    let mut tiers: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut flags_count: BTreeMap<&'static str, u64> = BTreeMap::new();
    for (u, ((dir, stem), members)) in left.units.iter().enumerate() {
        let kind = unit_kind(&left, members);
        let unindexed = members.iter().any(|&i| left.rows[i].sha.is_none());
        let ms: Vec<Option<&Match>> = members.iter().map(|&i| matches[i].as_ref()).collect();
        let matched = ms.iter().filter(|m| m.is_some()).count();
        let target = unit_target[u];
        let status: &'static str = if unindexed {
            "not-indexed"
        } else if matched == 0 && !right_complete {
            "not-found-yet"
        } else if matched == 0 {
            "left-only"
        } else if matched < members.len() {
            "partial"
        } else if members.iter().zip(&ms).any(|(&i, m)| m.unwrap().tier == Tier::UnitPartner && is_image(left.rows[i].kind)) {
            "image-differs"
        } else if ms.iter().all(|m| m.unwrap().tier == Tier::Identical) {
            "identical"
        } else {
            "metadata-differs"
        };
        let mut flags: Vec<&'static str> = Vec::new();
        if let Some(t) = target {
            let ((rdir, rstem), rmembers) = &right.units[t];
            if rdir != dir {
                flags.push("moved");
            }
            if rstem != stem {
                flags.push("renamed");
            }
            if rmembers.len() > members.len() {
                flags.push("right-has-more");
            }
            if ms.iter().flatten().any(|m| right.rows[m.right].unit != t) {
                flags.push("split");
            }
        }
        if ms.iter().flatten().any(|m| m.candidates > 1) {
            flags.push("duplicates");
        }
        for f in &flags {
            *flags_count.entry(f).or_default() += 1;
        }
        *by_kind.entry(kind.clone()).or_default().entry(status).or_default() += 1;
        let member_text: Vec<String> = members.iter().map(|&i| {
            let l = &left.rows[i];
            let ext = l.rel.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
            match &matches[i] {
                Some(m) => format!("{ext}:{}", tier_name(m.tier)),
                None if l.sha.is_none() => format!("{ext}:not-indexed"),
                None => format!("{ext}:unmatched"),
            }
        }).collect();
        let right_unit = target.map(|t| { let ((d, s), _) = &right.units[t]; format!("{d}/{s}") }).unwrap_or_default();
        writeln!(units_tsv, "{dir}/{stem}\t{kind}\t{status}\t{}\t{right_unit}\t{}", flags.join(","), member_text.join(" "))?;
        for &i in members {
            let l = &left.rows[i];
            match &matches[i] {
                Some(m) => {
                    let r = &right.rows[m.right];
                    *tiers.entry(tier_name(m.tier)).or_default() += 1;
                    writeln!(files_tsv, "{}\t{}\t{}\t{}", l.rel, r.rel, tier_name(m.tier), m.candidates)?;
                    let pair = format!("{}/{}\t{}/{}", left.root, l.rel, right.root, r.rel);
                    match (m.tier, l.kind) {
                        (Tier::Identical, _) | (_, Kind::Sidecar | Kind::Other) => {}
                        (Tier::UnitPartner, k) => writeln!(partner_pairs, "{}\t{pair}", kind_name(k))?,
                        (_, Kind::Dng) => writeln!(dng_pairs, "{pair}")?,
                        _ => writeln!(image_pairs, "{pair}")?,
                    }
                }
                None => {
                    *tiers.entry(if l.sha.is_none() { "not-indexed" } else { "unmatched" }).or_default() += 1;
                    writeln!(files_tsv, "{}\t\t{}\t0", l.rel, if l.sha.is_none() { "not-indexed" } else { "unmatched" })?;
                }
            }
        }
    }
    writeln!(right_only, "right_unit\tkind\tcontent")?;
    let left_shas: HashSet<&str> = left.rows.iter().filter_map(|r| r.sha.as_deref()).collect();
    let mut right_only_count: BTreeMap<String, u64> = BTreeMap::new();
    let (mut right_copies, mut right_unindexed) = (0u64, 0u64);
    for (u, ((dir, stem), members)) in right.units.iter().enumerate() {
        if used_right_units.contains(&u) {
            continue;
        }
        let indexed = members.iter().all(|&i| right.rows[i].sha.is_some());
        let kind = unit_kind(&right, members);
        let copy = indexed && members.iter().all(|&i| right.rows[i].sha.as_deref().is_some_and(|s| left_shas.contains(s)));
        let content = if !indexed {
            right_unindexed += 1;
            "not-indexed"
        } else if copy {
            right_copies += 1;
            "copy"
        } else {
            *right_only_count.entry(kind.clone()).or_default() += 1;
            "new"
        };
        writeln!(right_only, "{dir}/{stem}\t{kind}\t{content}")?;
    }
    let summary = serde_json::json!({
        "left": {"root": left.root, "files": left.rows.len(), "units": left.units.len()},
        "right": {"root": right.root, "files": right.rows.len(), "units": right.units.len(),
                  "files_not_yet_indexed": right.rows.iter().filter(|r| r.sha.is_none()).count()},
        "left_units_by_kind_and_status": by_kind,
        "left_unit_flags": flags_count,
        "left_files_by_tier": tiers,
        "right_only_new_units_by_kind": right_only_count,
        "right_only_copies_of_left_content": right_copies,
        "right_only_units_not_yet_indexed": right_unindexed,
    });
    fs::write(Path::new(&out).join("summary.json"), serde_json::to_string_pretty(&summary)? + "\n")?;
    for f in [&mut units_tsv, &mut files_tsv, &mut dng_pairs, &mut image_pairs, &mut partner_pairs, &mut right_only] {
        f.flush()?;
    }
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
