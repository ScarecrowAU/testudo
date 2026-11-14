use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bytes::BytesMut;
use testudo_protocol::{Event, Message, decode};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, warn};

use crate::hub::Hub;

/// Local socket that adapters connect to. One connection per adapter instance
/// (reporter, each Playwright worker's preload, ...). The first message on a
/// connection must be `Hello`, which names the run it belongs to.
///
/// Unix domain sockets only for now; Windows needs a named-pipe listener (the JS
/// transport already handles both via `net.connect(path)`).
pub struct IngestServer {
    path: PathBuf,
}

impl IngestServer {
    /// Bind the socket and start accepting. Must be called inside a tokio runtime.
    pub fn bind(hub: Hub) -> Result<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("testudo-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding ingest socket at {}", path.display()))?;

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let hub = hub.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, hub).await {
                                debug!("adapter connection ended: {e:#}");
                            }
                        });
                    }
                    Err(e) => warn!("ingest accept failed: {e}"),
                }
            }
        });

        Ok(Self { path })
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IngestServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn handle_connection(mut stream: UnixStream, hub: Hub) -> Result<()> {
    let mut buf = BytesMut::with_capacity(256 * 1024);
    let mut run_id: Option<String> = None;

    loop {
        loop {
            match decode(&mut buf) {
                Ok(Some(msg)) => {
                    if run_id.is_none() {
                        match &msg {
                            Message::Event(Event::Hello { run_id: Some(id), .. }) => {
                                run_id = Some(id.clone())
                            }
                            _ => anyhow::bail!("first message was not Hello with a runId"),
                        }
                    }
                    hub.publish(run_id.as_deref().unwrap(), msg);
                }
                Ok(None) => break,
                Err(testudo_protocol::DecodeError::Malformed(e)) => {
                    debug!("skipping malformed adapter message: {e}");
                }
                Err(e) => return Err(e.into()),
            }
        }
        if stream.read_buf(&mut buf).await? == 0 {
            return Ok(());
        }
    }
}
