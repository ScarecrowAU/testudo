//! Bakes in the target triple (which release archive fits this build) and the
//! git commit it was built from (what the bleeding-edge channel compares).

use std::process::Command;

fn main() {
    println!("cargo:rustc-env=TESTUDO_TARGET={}", std::env::var("TARGET").unwrap());
    println!("cargo:rerun-if-env-changed=TESTUDO_COMMIT");
    let commit = std::env::var("TESTUDO_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_default();
    println!("cargo:rustc-env=TESTUDO_COMMIT={commit}");
    // HEAD's reflog changes on every commit and checkout.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/logs/HEAD");
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    (out.status.success() && !text.trim().is_empty()).then(|| text.trim().to_owned())
}
