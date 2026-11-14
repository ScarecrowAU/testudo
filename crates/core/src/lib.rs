//! Testudo core: spawns test runners, ingests adapter streams over a local socket,
//! and fans messages out to consumers (GUI, recorder, storage).

pub mod discover;
pub mod hub;
pub mod ingest;
pub mod node_env;
pub mod procs;
pub mod runner;
pub mod settings;
pub mod shell_env;

pub use hub::{Hub, HubMessage, RunId, now_ms};
pub use ingest::IngestServer;
pub use runner::{CANCEL_GRACE, Orchestrator, RunHandle, RunSpec, RunnerKind};

use std::path::PathBuf;

/// Directory holding the JS adapters (`preload.mjs`, reporters).
///
/// Resolution order: `$TESTUDO_ADAPTERS_DIR`, `<exe dir>/adapters` (portable
/// bundle), `<exe dir>/../lib/testudo/adapters` (`make install` layout), then
/// the source tree (dev builds).
pub fn adapters_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TESTUDO_ADAPTERS_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| exe.parent().map(PathBuf::from))
    {
        for dir in [exe_dir.join("adapters"), exe_dir.join("../lib/testudo/adapters")] {
            if dir.join("preload.mjs").exists() {
                return dir.canonicalize().unwrap_or(dir);
            }
        }
    }
    let dev = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../adapters/js"));
    dev.canonicalize().unwrap_or(dev)
}
