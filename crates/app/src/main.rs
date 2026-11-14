mod app;
mod bridge;
mod model;
mod replay;
mod view;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use anyhow::Result;
use testudo_core::{Hub, IngestServer, Orchestrator, adapters_dir};
use testudo_store::{Recorder, Retention, Store, default_data_dir};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn".into()),
        )
        .init();

    // testudo [project-dir] [--run playwright|vitest] [--replay <run-id> [--at <secs>]] [-- runner args]
    let mut default_dir = None;
    let mut autorun = None;
    let mut replay = None;
    let mut replay_at = None;
    let mut runner_args: Vec<String> = Vec::new();
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--run" => autorun = argv.next(),
            "--replay" => replay = argv.next(),
            "--at" => replay_at = argv.next().and_then(|s| s.parse::<f64>().ok()),
            "--" => runner_args.extend(argv.by_ref()),
            _ => default_dir = Some(PathBuf::from(arg)),
        }
    }
    let default_dir = default_dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let hub = Hub::default();

    // History is best-effort: a broken store must not stop live runs.
    let data_dir = default_data_dir();
    let recorder = Recorder::spawn(&data_dir, &hub, Retention::from_env())
        .inspect_err(|e| tracing::error!("recording disabled: {e:#}"))
        .ok();
    let store = Store::open(&data_dir)
        .inspect_err(|e| tracing::error!("history unavailable: {e:#}"))
        .ok();

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

    // SIGINT/SIGTERM/SIGHUP close the window like the close button does, so the
    // shutdown below always runs. A second signal exits immediately.
    let ui_ctx: Arc<OnceLock<egui::Context>> = Arc::default();
    spawn_signal_handler(&rt, ui_ctx.clone());

    let handle = rt.handle().clone();
    let ui_hub = hub.clone();
    let ui_orchestrator = orchestrator.clone();
    eframe::run_native(
        "Testudo",
        options,
        Box::new(move |cc| {
            let _ = ui_ctx.set(cc.egui_ctx.clone());
            let rx = bridge::spawn(&handle, &ui_hub, cc.egui_ctx.clone());
            let mut app = app::TestudoApp::new(ui_orchestrator, rx, default_dir, store);
            app.set_args(&runner_args);
            if let Some(kind) = autorun.as_deref() {
                app.autorun(kind);
            }
            if let Some(id) = replay.as_deref() {
                app.open_replay(id);
                if let Some(secs) = replay_at {
                    app.seek_replay(&cc.egui_ctx, secs * 1000.0);
                }
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Window closed (button or signal): stop runs so no test processes outlive
    // Testudo, then let the recorder write their final status and exit.
    let active = orchestrator.active_runs();
    if active > 0 {
        tracing::info!(active, "stopping active runs");
        orchestrator.shutdown_blocking();
    }
    drop(ingest);
    hub.close();
    if let Some(r) = recorder {
        r.join();
    }
    Ok(())
}

fn spawn_signal_handler(rt: &tokio::runtime::Runtime, ctx: Arc<OnceLock<egui::Context>>) {
    rt.spawn(async move {
        let mut seen = 0;
        loop {
            let name = next_stop_signal().await;
            seen += 1;
            if seen > 1 {
                tracing::warn!("{name} again: exiting without cleanup");
                std::process::exit(130);
            }
            tracing::info!("{name}: closing (send again to force)");
            match ctx.get() {
                Some(ctx) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    ctx.request_repaint();
                }
                // Window not up yet: nothing to clean up.
                None => std::process::exit(130),
            }
        }
    });
}

#[cfg(unix)]
async fn next_stop_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut int), Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = int.recv() => "SIGINT",
        _ = term.recv() => "SIGTERM",
        _ = hup.recv() => "SIGHUP",
    }
}

#[cfg(not(unix))]
async fn next_stop_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl+C"
}
