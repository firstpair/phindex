// Bake the source commit and build time into the binary, for its launch header.
use std::process::Command;

fn out(cmd: &mut Command) -> Option<String> {
    let o = cmd.output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn main() {
    let commit = out(Command::new("git").args(["rev-parse", "HEAD"])).unwrap_or_else(|| "unknown".into());
    let dirty = !Command::new("git").args(["diff", "--quiet", "HEAD"]).status().is_ok_and(|s| s.success());
    let built = out(Command::new("date").env("TZ", "America/Los_Angeles").arg("+%a %Y-%m-%d %H:%M:%S %Z"))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=PHINDEX_COMMIT={commit}{}", if dirty { "-dirty" } else { "" });
    println!("cargo:rustc-env=PHINDEX_BUILT={built}");
    // A commit, a checkout or an edited source rebuilds the header.
    for p in ["../.git/HEAD", "../.git/index", "src", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={p}");
    }
}
