//! User settings shared by the GUI and the CLI, stored as JSON in
//! `$XDG_CONFIG_HOME/testudo/settings.json` (or `~/.config/testudo`;
//! `TESTUDO_CONFIG_DIR` overrides the directory). Unknown or missing fields
//! take their defaults, so older and newer files both load.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Recording resolutions offered in the settings menu: frames are scaled to
/// fit within an N×N box, keeping the page's shape.
pub const FRAME_SIZES: [u32; 5] = [640, 800, 1024, 1280, 1920];
pub const DEFAULT_FRAME_MAX: u32 = 1024;
/// Frame-rate caps offered: 60 means every repaint (browsers top out there).
pub const FRAME_RATES: [u32; 5] = [60, 30, 15, 10, 5];
pub const DEFAULT_FRAME_RATE: u32 = 60;

/// Where updates come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateChannel {
    /// Signed GitHub releases.
    #[default]
    Stable,
    /// Every new commit on `main`, built from source on this machine.
    BleedingEdge,
}

impl UpdateChannel {
    pub const ALL: [UpdateChannel; 2] = [UpdateChannel::Stable, UpdateChannel::BleedingEdge];

    pub fn name(self) -> &'static str {
        match self {
            UpdateChannel::Stable => "stable",
            UpdateChannel::BleedingEdge => "bleeding-edge",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Recorded frames fit within this many pixels on each side.
    pub frame_max_px: u32,
    /// At most this many recorded frames per second.
    pub frame_rate: u32,
    /// Where recordings and history are kept; None is the default data
    /// directory. Read at startup (TESTUDO_DATA_DIR still overrides).
    pub recordings_dir: Option<PathBuf>,
    /// Look for a newer Testudo at startup and once a day.
    pub check_updates: bool,
    pub update_channel: UpdateChannel,
    /// The last project folder scanned. The GUI reopens it when started
    /// without a folder from the home folder or `/` (Finder, the Dock).
    pub last_project_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            frame_max_px: DEFAULT_FRAME_MAX,
            frame_rate: DEFAULT_FRAME_RATE,
            recordings_dir: None,
            check_updates: true,
            update_channel: UpdateChannel::Stable,
            last_project_dir: None,
        }
    }
}

impl Settings {
    /// The recording box size, kept to something a browser can produce.
    pub fn frame_max(&self) -> u32 {
        self.frame_max_px.clamp(160, 4096)
    }

    pub fn frame_rate(&self) -> u32 {
        self.frame_rate.clamp(1, 60)
    }

    /// Settings from the user's file; defaults when missing or unreadable.
    pub fn load() -> Self {
        Self::load_from(&path())
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), "settings unreadable, using defaults: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&path())
    }

    /// Write atomically (temp file + rename) so a crash never leaves half a file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("settings path has no directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

pub fn path() -> PathBuf {
    let dir = std::env::var_os("TESTUDO_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
                .join("testudo")
        });
    dir.join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_missing_or_bad_files() {
        let dir = std::env::temp_dir().join(format!("testudo-settings-{}", std::process::id()));
        let file = dir.join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Settings::load_from(&file), Settings::default(), "missing file");

        let s = Settings {
            frame_max_px: 1280,
            frame_rate: 15,
            recordings_dir: Some("/data/rec".into()),
            check_updates: false,
            update_channel: UpdateChannel::BleedingEdge,
            last_project_dir: Some("/work/app".into()),
        };
        s.save_to(&file).unwrap();
        assert_eq!(Settings::load_from(&file), s);
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains(r#""update_channel": "bleeding-edge""#)
        );

        std::fs::write(&file, "{}").unwrap();
        assert_eq!(
            Settings::load_from(&file).frame_max_px,
            DEFAULT_FRAME_MAX,
            "missing field"
        );
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(Settings::load_from(&file), Settings::default(), "bad file");
        assert_eq!(
            Settings {
                frame_max_px: 10,
                ..Default::default()
            }
            .frame_max(),
            160
        );
        assert_eq!(
            Settings {
                frame_rate: 0,
                ..Default::default()
            }
            .frame_rate(),
            1
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
