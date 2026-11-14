mod app;
mod bridge;
mod fonts;
mod logic;
mod model;
mod perf;
mod state;
mod telemetry;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Result;
use futures::StreamExt;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;
use testudo_core::{Hub, IngestServer, Orchestrator, adapters_dir};
use testudo_store::{Recorder, Retention, Store, default_data_dir};

fn main() -> Result<()> {
    // Desktop launchers don't run the user's shell rc files, so runners would
    // miss nvm/Homebrew/pnpm on PATH. Must happen before any threads start.
    // SAFETY: single-threaded at this point.
    let imported = unsafe { testudo_core::shell_env::import_login_shell_env() };

    // Lives until the end of main so OTLP export is flushed on exit.
    let telemetry = telemetry::init();
    if let Some(path) = &telemetry.log_file {
        tracing::info!(path = %path.display(), "logging to file");
    }
    if let Some(endpoint) = &telemetry.otlp_endpoint {
        tracing::info!(%endpoint, "exporting traces over OTLP");
    }

    // testudo [project-dir] [--run all|playwright|vitest|<suite name part>]
    //         [--replay <run-id> [--at <secs>] [--focus <page-id>]] [-- runner args]
    let mut startup = app::Startup::default();
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--run" => startup.autorun = argv.next(),
            "--replay" => startup.replay = argv.next(),
            "--focus" => startup.focus_page = argv.next(),
            "--at" => startup.replay_at = argv.next().and_then(|s| s.parse::<f64>().ok()),
            "--" => startup.runner_args.extend(argv.by_ref()),
            _ => startup.project_dir = Some(PathBuf::from(arg)),
        }
    }
    let project_dir = startup
        .project_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    startup.project_dir = Some(project_dir);

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let hub = Hub::default();

    // History is best-effort: a broken store must not stop live runs.
    let data_dir = default_data_dir();
    let recorder = Recorder::spawn(&data_dir, &hub, Retention::from_env())
        .inspect_err(|e| tracing::error!("recording disabled: {e:#}"))
        .ok();
    startup.store = Store::open(&data_dir)
        .inspect_err(|e| tracing::error!("history unavailable: {e:#}"))
        .ok()
        .map(Rc::new);

    let ingest = {
        let _g = rt.enter();
        IngestServer::bind(hub.clone())?
    };
    let orchestrator = Orchestrator::new(&ingest, hub.clone(), adapters_dir(), rt.handle().clone());
    tracing::info!(socket = %ingest.socket_path().display(), adapters = %adapters_dir().display(), imported_shell_vars = imported, "testudo ready");

    startup.orchestrator = Some(orchestrator.clone());
    startup.rt = Some(rt.handle().clone());
    startup.ui_rx = Some(bridge::spawn(rt.handle(), &hub));

    // SIGINT/SIGTERM/SIGHUP quit the app like closing the window, so the
    // shutdown below always runs. A second signal exits immediately.
    let (sig_tx, mut sig_rx) = futures::channel::mpsc::unbounded::<&'static str>();
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
            let _ = sig_tx.unbounded_send(name);
        }
    });

    let startup = Rc::new(Cell::new(Some(startup)));
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        {
            let theme = Theme::global_mut(cx);
            theme.font_family = fonts::sans();
            theme.mono_font_family = fonts::mono();
        }
        app::bind_keys(cx);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1400.), px(900.)), cx))),
            titlebar: Some(TitlebarOptions {
                title: Some("Testudo".into()),
                ..Default::default()
            }),
            app_id: Some("testudo".into()),
            ..Default::default()
        };
        let startup = startup.clone();
        let opened = gpui_kit::open_window(options, cx, move |window, cx| {
            let s = startup.take().expect("window opened once");
            cx.new(|cx| app::TestudoApp::new(s, window, cx))
        });
        if let Err(e) = opened {
            tracing::error!("failed to open window: {e:#}");
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.spawn(async move |cx| {
            if sig_rx.next().await.is_some() {
                cx.update(|cx| cx.quit());
            }
        })
        .detach();
    });

    // Window closed or signalled: stop runs so no test processes outlive
    // Testudo, then let the recorder write their final status.
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
    drop(telemetry);
    Ok(())
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
