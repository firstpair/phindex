//! `phindex diff`: the files of one tree that are not aligned with another.
//!
//!   phindex diff --left A.sqlite --left-root DIR --right B.sqlite --right-root DIR --out DIFF_DIR [--states LIST]
//!
//! Every file under the left root gets one state, from the two indexes alone:
//!
//! | state | the right tree has | |
//! |---|---|---|
//! | `same` | the same relative path, the same bytes | aligned |
//! | `same-image` | the same path, other bytes, the same raw blocks or image data | aligned |
//! | `changed` | the same path, other bytes, no shared image identity | |
//! | `unread` | the same path, but one of the two rows is not read yet | |
//! | `copy` | no such path; the same bytes somewhere else | |
//! | `variant` | no such path; the same raw blocks or image data somewhere else | |
//! | `only-left` | no such path, and nothing with its content | |
//! | `unknown` | no such path, and the left row is not read yet | |
//!
//! `diff.tsv` lists the files in the states asked for with `--states`, by
//! default the four without the path on the right: `copy`, `variant`,
//! `only-left` and `unknown`. Once those are moved away (`phindex mv --list`),
//! the left tree is a subset of the right one by path. A sidecar edited on one
//! side is `changed`, as is a re-rendered JPEG; name `changed` in `--states`
//! to list them too.
//!
//! The list starts with a line naming the left root, then a header, then one
//! line per file: its path relative to the root, state, size, modification
//! time and SHA-256. The summary, with files and bytes for every state, is
//! written to `summary.json` and to standard output. Only the indexes are
//! read; no file of either tree is opened.
use crate::conform::{get, open_ro};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

pub(crate) const LIST: &str = "phindex-diff";
const STATES: [&str; 8] = ["same", "same-image", "changed", "unread", "copy", "variant", "only-left", "unknown"];
const USAGE: &str = "usage: phindex diff --left A.sqlite --left-root DIR --right B.sqlite --right-root DIR --out DIFF_DIR [--states LIST]\n       \
                     states: same, same-image, changed, unread, copy, variant, only-left, unknown (default: the last four)";

pub fn run(args: &[String]) -> Result<()> {
    let need = |flag: &str| get(args, flag).with_context(|| format!("{flag} required\n{USAGE}"));
    let (left, right, out) = (need("--left")?, need("--right")?, need("--out")?);
    let (left_root, right_root) = (need("--left-root")?.trim_end_matches('/').to_string(), need("--right-root")?.trim_end_matches('/').to_string());
    let wanted: Vec<String> = get(args, "--states").unwrap_or_else(|| "copy,variant,only-left,unknown".into()).split(',').map(String::from).collect();
    if let Some(bad) = wanted.iter().find(|s| !STATES.contains(&s.as_str())) {
        bail!("{bad}: not a state\n{USAGE}");
    }
    let list_path = Path::new(&out).join("diff.tsv");
    if list_path.exists() {
        bail!("{} exists: a list is not overwritten, since a journal may refer to it", list_path.display());
    }

    // The right tree: each path's content, and every content's identities.
    let mut there: HashMap<String, (Option<String>, Option<String>, Option<String>)> = HashMap::new();
    let (mut shas, mut images): (HashSet<String>, HashSet<String>) = Default::default();
    {
        let c = open_ro(&right)?;
        let mut st = c.prepare("SELECT path, sha256, dng_blocks, image_data FROM files WHERE gone=0 AND path>=?1 AND path<?2")?;
        let mut q = st.query([format!("{right_root}/"), format!("{right_root}0")])?;
        while let Some(r) = q.next()? {
            let path: String = r.get(0)?;
            let row: (Option<String>, Option<String>, Option<String>) = (r.get(1)?, r.get(2)?, r.get(3)?);
            shas.extend(row.0.clone());
            images.extend(row.1.clone());
            images.extend(row.2.clone());
            there.insert(path[right_root.len() + 1..].to_string(), row);
        }
    }
    if there.is_empty() {
        bail!("{right} lists no files under {right_root}");
    }

    fs::create_dir_all(&out)?;
    let mut list = BufWriter::new(fs::File::create(&list_path)?);
    writeln!(list, "{LIST}\t1\t{left_root}")?;
    writeln!(list, "path\tstate\tsize\tmtime_ns\tsha256")?;
    let mut count: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    let (mut total, mut listed, mut listed_bytes) = (0u64, 0u64, 0u64);
    let c = open_ro(&left)?;
    let mut st = c.prepare("SELECT path, size, mtime_ns, sha256, dng_blocks, image_data FROM files WHERE gone=0 AND path>=?1 AND path<?2 ORDER BY path")?;
    let mut q = st.query([format!("{left_root}/"), format!("{left_root}0")])?;
    while let Some(r) = q.next()? {
        let path: String = r.get(0)?;
        let name = &path[left_root.len() + 1..];
        let (size, mtime, sha): (i64, i64, Option<String>) = (r.get(1)?, r.get(2)?, r.get(3)?);
        let ids: [Option<String>; 2] = [r.get(4)?, r.get(5)?];
        let state: &'static str = match (there.get(name), &sha) {
            (Some(_), None) | (Some((None, _, _)), _) => "unread",
            (Some((Some(s), _, _)), Some(mine)) if s == mine => "same",
            (Some((_, blocks, image)), Some(_)) => {
                if (ids[0].is_some() && ids[0] == *blocks) || (ids[1].is_some() && ids[1] == *image) { "same-image" } else { "changed" }
            }
            (None, None) => "unknown",
            (None, Some(mine)) if shas.contains(mine) => "copy",
            (None, Some(_)) if ids.iter().flatten().any(|v| images.contains(v)) => "variant",
            (None, Some(_)) => "only-left",
        };
        total += 1;
        let e = count.entry(state).or_default();
        *e = (e.0 + 1, e.1 + size as u64);
        if wanted.iter().any(|w| w == state) {
            if name.contains(['\t', '\n']) {
                bail!("{path}: a name with a tab or a line break cannot be listed");
            }
            listed += 1;
            listed_bytes += size as u64;
            writeln!(list, "{name}\t{state}\t{size}\t{mtime}\t{}", sha.as_deref().unwrap_or(""))?;
        }
    }
    if total == 0 {
        bail!("{left} lists no files under {left_root}");
    }
    list.flush()?;
    let states: BTreeMap<&str, serde_json::Value> = count.iter().map(|(k, (f, b))| (*k, serde_json::json!({"files": f, "bytes": b}))).collect();
    let summary = serde_json::json!({
        "left": {"root": left_root, "files": total},
        "right": {"root": right_root, "files": there.len()},
        "left_files_by_state": states,
        "states_listed": wanted,
        "files_listed": listed,
        "bytes_listed": listed_bytes,
        "list": list_path.to_str(),
    });
    fs::write(Path::new(&out).join("summary.json"), serde_json::to_string_pretty(&summary)? + "\n")?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
