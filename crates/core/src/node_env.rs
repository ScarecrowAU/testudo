//! Pick the Node.js a project asks for (`.nvmrc` / `.node-version`) from locally
//! installed version managers, so runs use the project's Node rather than
//! whatever happens to be on Testudo's own PATH.
//!
//! Supported installs: nvm (`$NVM_DIR` or `~/.nvm`), fnm (`$FNM_DIR` or
//! `~/.local/share/fnm`). Exact version wins; otherwise the newest install with
//! the same major (and minor, if specified). Aliases like `lts/*` are not resolved.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeChoice {
    pub wanted: String,
    pub source: PathBuf,
    pub version: String,
    pub bin_dir: PathBuf,
    pub exact: bool,
}

/// Find the version file nearest to `cwd` (walking up) and a matching install.
/// `Err` carries a human-readable reason when a version file exists but no
/// install matches.
pub fn resolve(cwd: &Path) -> Option<Result<NodeChoice, String>> {
    let (source, wanted) = find_version_file(cwd)?;
    let want = parse_version(&wanted);
    let Some(want) = want else {
        return Some(Err(format!(
            "{} asks for '{wanted}', which Testudo can't resolve (aliases are unsupported)",
            source.display()
        )));
    };

    let mut best: Option<((u64, u64, u64), String, PathBuf)> = None;
    for (version, bin_dir) in installed() {
        let Some(v) = parse_version(&version) else { continue };
        let matches = v.0 == want.0 && want.1.is_none_or(|m| v.1 == Some(m)) && want.2.is_none_or(|p| v.2 == Some(p));
        let full = (v.0, v.1.unwrap_or(0), v.2.unwrap_or(0));
        if matches && best.as_ref().is_none_or(|(b, _, _)| full > *b) {
            best = Some((full, version, bin_dir));
        }
    }
    // Fall back to the newest install of the same major when the exact
    // patch/minor isn't installed.
    if best.is_none() {
        for (version, bin_dir) in installed() {
            let Some(v) = parse_version(&version) else { continue };
            let full = (v.0, v.1.unwrap_or(0), v.2.unwrap_or(0));
            if v.0 == want.0 && best.as_ref().is_none_or(|(b, _, _)| full > *b) {
                best = Some((full, version, bin_dir));
            }
        }
    }

    Some(match best {
        Some((full, version, bin_dir)) => Ok(NodeChoice {
            exact: want.1.is_some_and(|m| m == full.1) && want.2.is_some_and(|p| p == full.2),
            wanted,
            source,
            version,
            bin_dir,
        }),
        None => Err(format!(
            "{} asks for Node {wanted}, but no matching nvm/fnm install was found; using PATH",
            source.display()
        )),
    })
}

fn find_version_file(cwd: &Path) -> Option<(PathBuf, String)> {
    for dir in cwd.ancestors() {
        for name in [".nvmrc", ".node-version"] {
            let path = dir.join(name);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let wanted = text.lines().next().unwrap_or("").trim().to_owned();
                if !wanted.is_empty() {
                    return Some((path, wanted));
                }
            }
        }
    }
    None
}

/// `v26.12.1`, `26.12`, `26` -> (26, Some(12), Some(1)) etc.
fn parse_version(s: &str) -> Option<(u64, Option<u64>, Option<u64>)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map(str::parse).transpose().ok()?;
    let patch = parts.next().map(str::parse).transpose().ok()?;
    Some((major, minor, patch))
}

fn installed() -> Vec<(String, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let nvm = std::env::var_os("NVM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".nvm"))
        .join("versions/node");
    let fnm = std::env::var_os("FNM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share/fnm"))
        .join("node-versions");

    let mut out = Vec::new();
    for (root, bin_suffix) in [(nvm, "bin"), (fnm, "installation/bin")] {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for e in entries.flatten() {
            let bin = e.path().join(bin_suffix);
            if bin.join("node").exists() {
                out.push((e.file_name().to_string_lossy().into_owned(), bin));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("v26.12.1"), Some((26, Some(12), Some(1))));
        assert_eq!(parse_version("26"), Some((26, None, None)));
        assert_eq!(parse_version("lts/*"), None);
    }

    #[test]
    fn resolves_against_fake_nvm() {
        let root = std::env::temp_dir().join(format!("testudo-node-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for v in ["v24.1.0", "v26.8.1", "v26.9.0", "v27.0.0"] {
            let bin = root.join("nvm/versions/node").join(v).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("node"), "").unwrap();
        }
        let project = root.join("proj/sub");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(root.join("proj/.nvmrc"), "26.12.1\n").unwrap();

        // SAFETY: test-only env mutation; no other test reads NVM_DIR/FNM_DIR.
        unsafe {
            std::env::set_var("NVM_DIR", root.join("nvm"));
            std::env::set_var("FNM_DIR", root.join("none"));
        }
        let choice = resolve(&project).unwrap().unwrap();
        assert_eq!(choice.version, "v26.9.0", "newest same-major when exact is missing");
        assert!(!choice.exact);

        std::fs::write(root.join("proj/.nvmrc"), "v26.8.1").unwrap();
        let choice = resolve(&project).unwrap().unwrap();
        assert_eq!((choice.version.as_str(), choice.exact), ("v26.8.1", true));

        std::fs::write(root.join("proj/.nvmrc"), "22").unwrap();
        assert!(resolve(&project).unwrap().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
