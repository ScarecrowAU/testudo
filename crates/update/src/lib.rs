//! Updating Testudo in place.
//!
//! Two channels (see [`UpdateChannel`]): `stable` installs the latest GitHub
//! release after checking its minisign signature against the key built into
//! this binary; `bleeding-edge` builds the newest commit on `main` from a
//! checkout under `~/.cache/testudo/src` and installs that. Either way the
//! binaries and adapters are replaced where this build was installed (see
//! [`Install`]), and the running process keeps its old copy until restarted.

use std::time::Duration;

use anyhow::Result;
pub use testudo_core::settings::UpdateChannel;

mod install;
mod release;
mod source;

pub use install::{Bundle, Install, NotUpdatable};

/// The GitHub repository updates come from.
pub const REPO: &str = "ScarecrowAU/testudo";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The target triple this was built for; release archives are named by it.
pub const TARGET: &str = env!("TESTUDO_TARGET");
/// The commit this was built from; empty when unknown.
pub const COMMIT: &str = env!("TESTUDO_COMMIT");

/// Release archives are signed with the matching secret key in CI.
const PUBLIC_KEY: &str = "RWSmCfcnTJgsZrV6+H0NIc6Fi9I+ydhUDLazPG5hFYvAXpZtkmNcOTeJ";

/// A newer Testudo than this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub channel: UpdateChannel,
    /// What it updates to: "0.2.0", or "main @ 1a2b3c4".
    pub label: String,
    /// Its release notes, or the commits since this build.
    pub url: String,
    kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Release { archive_url: String, sig_url: String },
    Commit { sha: String },
}

/// This build, for display: "0.1.0 (1a2b3c4)".
pub fn current() -> String {
    match COMMIT.get(..7) {
        Some(short) => format!("{VERSION} ({short})"),
        None => VERSION.to_owned(),
    }
}

/// Ask GitHub whether `channel` has something newer than this build.
pub fn check(channel: UpdateChannel) -> Result<Option<Update>> {
    match channel {
        UpdateChannel::Stable => release::check(),
        UpdateChannel::BleedingEdge => source::check(),
    }
}

/// Download (stable) or build (bleeding-edge) `update` and install it over
/// `to`. `log` gets one line per step, and the build's output.
pub fn install(update: &Update, to: &Install, log: &mut dyn FnMut(&str)) -> Result<()> {
    let bundle = match &update.kind {
        Kind::Release { archive_url, sig_url } => release::fetch(archive_url, sig_url, log)?,
        Kind::Commit { sha } => source::build(sha, log)?,
    };
    log("Installing");
    to.replace(&bundle)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .user_agent(format!("testudo/{VERSION}"))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_per_read(Some(Duration::from_secs(30)))
        .build()
        .into()
}
