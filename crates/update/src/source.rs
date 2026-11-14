//! The bleeding-edge channel: build the newest commit on `main`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use crate::install::{BINARIES, Bundle};
use crate::{COMMIT, Kind, REPO, Update, UpdateChannel, agent};

pub fn check() -> Result<Option<Update>> {
    let url = format!("https://api.github.com/repos/{REPO}/commits/main");
    let head = agent()
        .get(&url)
        .header("Accept", "application/vnd.github.sha")
        .call()
        .map_err(|e| anyhow!(e).context("asking GitHub for the newest commit"))?
        .body_mut()
        .read_to_string()?;
    Ok(newer(head.trim(), COMMIT))
}

fn newer(head: &str, current: &str) -> Option<Update> {
    if head.is_empty() || head == current {
        return None;
    }
    let url = if current.is_empty() {
        format!("https://github.com/{REPO}/commits/main")
    } else {
        format!("https://github.com/{REPO}/compare/{current}...{head}")
    };
    Some(Update {
        channel: UpdateChannel::BleedingEdge,
        label: format!("main @ {}", &head[..head.len().min(7)]),
        url,
        kind: Kind::Commit { sha: head.to_owned() },
    })
}

/// The checkout builds happen in; kept so later builds are incremental.
pub fn src_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("testudo/src")
}

pub fn build(sha: &str, log: &mut dyn FnMut(&str)) -> Result<Bundle> {
    for tool in ["git", "cargo"] {
        Command::new(tool)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| anyhow!("building from source needs {tool} on PATH"))?;
    }
    let dir = src_dir();
    if !dir.join(".git").is_dir() {
        log("Cloning");
        std::fs::create_dir_all(dir.parent().unwrap())?;
        let _ = std::fs::remove_dir_all(&dir);
        let url = format!("https://github.com/{REPO}.git");
        run(
            Command::new("git")
                .args(["clone", "--filter=blob:none", &url])
                .arg(&dir),
            log,
        )?;
    }
    log("Fetching main");
    run(git(&dir).args(["fetch", "--quiet", "origin", "main"]), log)?;
    run(git(&dir).args(["checkout", "--quiet", "--force", "--detach", sha]), log)?;
    log("Building (this takes a few minutes)");
    run(
        Command::new("cargo")
            .args([
                "build",
                "--release",
                "--locked",
                "-p",
                "testudo-ui",
                "-p",
                "testudo-cli",
            ])
            .current_dir(&dir)
            .env("TESTUDO_COMMIT", sha)
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_TERM_PROGRESS_WHEN", "never"),
        log,
    )?;
    collect(&dir)
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    c
}

/// Run `cmd`, passing its stderr (where git and cargo report) to `log`.
fn run(cmd: &mut Command, log: &mut dyn FnMut(&str)) -> Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {cmd:?}"))?;
    let mut tail = Vec::new();
    for line in BufReader::new(child.stderr.take().unwrap()).lines() {
        let line = line?;
        log(&line);
        tail.push(line);
        if tail.len() > 20 {
            tail.remove(0);
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!(
            "{} failed ({status}):\n{}",
            cmd.get_program().to_string_lossy(),
            tail.join("\n")
        );
    }
    Ok(())
}

/// The built binaries and the checkout's adapters.
fn collect(dir: &Path) -> Result<Bundle> {
    let mut bundle = Bundle::default();
    for name in BINARIES {
        let path = dir.join("target/release").join(name);
        bundle.binaries.push((
            name.to_owned(),
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        ));
    }
    let adapters = dir.join("adapters/js");
    for entry in std::fs::read_dir(&adapters).with_context(|| format!("reading {}", adapters.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file() && (name.ends_with(".mjs") || name == "package.json") {
            bundle.adapters.push((name, std::fs::read(entry.path())?));
        }
    }
    bundle.check()?;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_different_head_is_an_update() {
        let head = "1a2b3c4d5e6f";
        assert_eq!(newer(head, head), None);
        assert_eq!(newer("", "abc"), None);
        let u = newer(head, "0ff1ce").unwrap();
        assert_eq!(u.label, "main @ 1a2b3c4");
        assert!(u.url.ends_with("/compare/0ff1ce...1a2b3c4d5e6f"));
        assert!(
            newer(head, "").unwrap().url.ends_with("/commits/main"),
            "unknown build commit"
        );
    }

    #[test]
    fn collects_a_build_from_a_checkout() {
        let dir = std::env::temp_dir().join(format!("testudo-update-collect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target/release")).unwrap();
        std::fs::create_dir_all(dir.join("adapters/js")).unwrap();
        for name in BINARIES {
            std::fs::write(dir.join("target/release").join(name), name).unwrap();
        }
        for name in ["preload.mjs", "package.json", "README.md"] {
            std::fs::write(dir.join("adapters/js").join(name), name).unwrap();
        }
        let mut b = collect(&dir).unwrap();
        b.adapters.sort();
        assert_eq!(b.binaries.len(), 2);
        assert_eq!(
            b.adapters.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["package.json", "preload.mjs"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
