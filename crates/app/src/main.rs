mod app;
mod bridge;
mod model;

use std::path::PathBuf;

use anyhow::Result;
use testudo_core::{Hub, IngestServer, Orchestrator, adapters_dir};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn".into()),
        )
        .init();

    // testudo [project-dir] [--run playwright|vitest]
    let mut default_dir = None;
    let mut autorun = None;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--run" => autorun = argv.next(),
            _ => default_dir = Some(PathBuf::from(arg)),
        }
    }
    let default_dir = default_dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let hub = Hub::default();
    let ingest = {
        let _g = rt.enter();
        IngestServer::bind(hub.clone())?
    };
    let orchestrator = Orchestrator::new(
        hub.clone(),
        ingest.socket_path().to_owned(),
        adapters_dir(),
        rt.handle().clone(),
    );
    tracing::info!(socket = %ingest.socket_path().display(), adapters = %adapters_dir().display(), "testudo ready");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Testudo")
            .with_inner_size([1400.0, 900.0]),
        ..Default::default()
    };

    let handle = rt.handle().clone();
    eframe::run_native(
        "Testudo",
        options,
        Box::new(move |cc| {
            let rx = bridge::spawn(&handle, &hub, cc.egui_ctx.clone());
            let mut app = app::TestudoApp::new(orchestrator, rx, default_dir);
            if let Some(kind) = autorun.as_deref() {
                app.autorun(kind);
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    drop(ingest);
    Ok(())
}
