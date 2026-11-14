//! Where this build is installed, and replacing its files.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The binaries every install has.
pub const BINARIES: [&str; 2] = ["testudo", "testudo-cli"];

/// The files an update installs.
#[derive(Debug, Default)]
pub struct Bundle {
    /// (name, contents) for each of [`BINARIES`].
    pub binaries: Vec<(String, Vec<u8>)>,
    /// (file name, contents) of the JS adapters.
    pub adapters: Vec<(String, Vec<u8>)>,
}

impl Bundle {
    /// Every binary and the preload are present.
    pub fn check(&self) -> Result<()> {
        for name in BINARIES {
            if !self.binaries.iter().any(|(n, _)| n == name) {
                bail!("the update has no {name} binary");
            }
        }
        if !self.adapters.iter().any(|(n, _)| n == "preload.mjs") {
            bail!("the update has no adapters");
        }
        Ok(())
    }
}

/// An install this process can update: the portable layout (binaries with
/// `adapters/` beside them) or `make install`'s (`bin/` and
/// `lib/testudo/adapters`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub bin_dir: PathBuf,
    pub adapters_dir: PathBuf,
}

/// Why this build can't update itself; shown instead of an Update button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotUpdatable(pub String);

impl fmt::Display for NotUpdatable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotUpdatable {}

impl Install {
    /// The install the running binary belongs to.
    pub fn detect() -> Result<Self, NotUpdatable> {
        let exe = std::env::current_exe()
            .and_then(|e| e.canonicalize())
            .map_err(|e| NotUpdatable(format!("Couldn't find this program's location: {e}")))?;
        Self::at(exe.parent().unwrap_or(Path::new("/")))
    }

    fn at(bin_dir: &Path) -> Result<Self, NotUpdatable> {
        if bin_dir.join("deps").is_dir() && bin_dir.join(".fingerprint").is_dir() {
            return Err(NotUpdatable(
                "This is a development build in a Cargo target directory. Update its checkout instead.".into(),
            ));
        }
        let adapters_dir = [bin_dir.join("adapters"), bin_dir.join("../lib/testudo/adapters")]
            .into_iter()
            .find(|d| d.join("preload.mjs").is_file())
            .ok_or_else(|| NotUpdatable("Couldn't find the installed adapters next to this program.".into()))?;
        let adapters_dir = adapters_dir.canonicalize().unwrap_or(adapters_dir);
        for dir in [bin_dir, &adapters_dir] {
            if !writable(dir) {
                return Err(NotUpdatable(format!(
                    "{} isn't writable. Update Testudo the way it was installed.",
                    dir.display()
                )));
            }
        }
        Ok(Self {
            bin_dir: bin_dir.to_path_buf(),
            adapters_dir,
        })
    }

    /// Path of an installed binary (also after it was replaced, unlike
    /// `current_exe`, which then names the deleted file on Linux).
    pub fn exe(&self, name: &str) -> PathBuf {
        self.bin_dir.join(name)
    }

    /// Install `bundle`'s files. Everything is written beside its target
    /// first and only then renamed over it, so a full disk or a failed write
    /// leaves the install as it was; a running Testudo keeps its open copy.
    pub fn replace(&self, bundle: &Bundle) -> Result<()> {
        bundle.check()?;
        let files = bundle
            .binaries
            .iter()
            .map(|(n, data)| (self.bin_dir.join(n), data, 0o755))
            .chain(
                bundle
                    .adapters
                    .iter()
                    .map(|(n, data)| (self.adapters_dir.join(n), data, 0o644)),
            );
        let mut staged = Vec::new();
        for (dest, data, mode) in files {
            let tmp = dest.with_file_name(format!(".{}.update", dest.file_name().unwrap().to_string_lossy()));
            let written = write_mode(&tmp, data, mode);
            staged.push((tmp, dest));
            if let Err(e) = written {
                for (tmp, _) in &staged {
                    let _ = std::fs::remove_file(tmp);
                }
                return Err(e);
            }
        }
        for (tmp, dest) in staged {
            std::fs::rename(&tmp, &dest).with_context(|| format!("replacing {}", dest.display()))?;
        }
        Ok(())
    }
}

fn write_mode(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".testudo-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(tag: &str) -> Bundle {
        Bundle {
            binaries: BINARIES.iter().map(|n| (n.to_string(), tag.into())).collect(),
            adapters: vec![("preload.mjs".into(), tag.into()), ("reporter.mjs".into(), tag.into())],
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("testudo-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical, as detected paths are (macOS's temp dir is behind a symlink).
        dir.canonicalize().unwrap()
    }

    #[test]
    fn detects_both_layouts_and_refuses_cargo_targets() {
        let root = scratch("layouts");
        let portable = root.join("portable");
        std::fs::create_dir_all(portable.join("adapters")).unwrap();
        std::fs::write(portable.join("adapters/preload.mjs"), "").unwrap();
        assert_eq!(Install::at(&portable).unwrap().adapters_dir, portable.join("adapters"));

        let prefix = root.join("prefix");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("lib/testudo/adapters")).unwrap();
        std::fs::write(prefix.join("lib/testudo/adapters/preload.mjs"), "").unwrap();
        assert_eq!(
            Install::at(&prefix.join("bin")).unwrap().adapters_dir,
            prefix.join("lib/testudo/adapters")
        );

        let target = root.join("target/release");
        std::fs::create_dir_all(target.join("deps")).unwrap();
        std::fs::create_dir_all(target.join(".fingerprint")).unwrap();
        assert!(Install::at(&target).unwrap_err().0.contains("development build"));
        assert!(Install::at(&root.join("nowhere")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaces_every_file_and_leaves_no_staging_behind() {
        let dir = scratch("replace");
        std::fs::create_dir_all(dir.join("adapters")).unwrap();
        let install = Install {
            bin_dir: dir.clone(),
            adapters_dir: dir.join("adapters"),
        };
        install.replace(&bundle("old")).unwrap();
        install.replace(&bundle("new")).unwrap();
        for path in [
            "testudo",
            "testudo-cli",
            "adapters/preload.mjs",
            "adapters/reporter.mjs",
        ] {
            assert_eq!(std::fs::read_to_string(dir.join(path)).unwrap(), "new", "{path}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("testudo")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".update"))
            .collect();
        assert!(leftovers.is_empty());

        let mut partial = bundle("bad");
        partial.binaries.pop();
        assert!(install.replace(&partial).is_err());
        assert_eq!(std::fs::read_to_string(dir.join("testudo")).unwrap(), "new");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
