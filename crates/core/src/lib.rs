//! Testudo core: spawns test runners, ingests adapter streams over a local socket,
//! and fans messages out to consumers (GUI, recorder, storage).

pub mod hub;
pub mod ingest;
pub mod runner;

pub use hub::{Hub, HubMessage, RunId};
pub use ingest::IngestServer;
pub use runner::{Orchestrator, RunHandle, RunSpec, RunnerKind};

use std::path::PathBuf;

/// Directory holding the JS adapters (`preload.mjs`, reporters).
///
/// Resolution order: `$TESTUDO_ADAPTERS_DIR`, `<exe dir>/adapters`, then the source
/// tree (dev builds).
pub fn adapters_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TESTUDO_ADAPTERS_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("adapters")))
        .filter(|d| d.join("preload.mjs").exists())
    {
        return dir;
    }
    let dev = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../adapters/js"));
    dev.canonicalize().unwrap_or(dev)
}
