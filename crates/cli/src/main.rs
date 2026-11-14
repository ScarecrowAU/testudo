//! Headless Testudo. Seed of the CI agent.
//!
//! testudo-cli run <playwright|vitest|cmd:PROGRAM> <project-dir> [--no-record] [--quiet] [-- runner args...]
//! testudo-cli history [--limit N]
//! testudo-cli show <run-id>
//! testudo-cli export <run-id> <page-id> <out.mp4> [--fps N]
//! testudo-cli prune [--keep N] [--max-frames-mb N]

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use testudo_core::{Hub, IngestServer, Orchestrator, RunSpec, RunnerKind, adapters_dir};
use testudo_protocol::{Event, Message};
use testudo_store::export::{ExportOptions, export_mp4};
use testudo_store::{Recorder, Retention, Store, default_data_dir};

const USAGE: &str = "usage:
  testudo-cli run <playwright|vitest|cmd:PROGRAM> <project-dir> [--no-record] [--quiet] [-- runner args...]
  testudo-cli history [--limit N]
  testudo-cli show <run-id>
  testudo-cli export <run-id> <page-id> <out.mp4> [--fps N]
  testudo-cli prune [--keep N] [--max-frames-mb N]

data dir: $TESTUDO_DATA_DIR, else $XDG_DATA_HOME/testudo, else ~/.local/share/testudo
retention: TESTUDO_KEEP_RUNS (200), TESTUDO_MAX_FRAMES_MB (5120), TESTUDO_KEEP_FRAMES=all|failed";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let data = default_data_dir();
    match args.first().map(String::as_str) {
        Some("run") => {
            let code = run(&args[1..], &data)?;
            std::process::exit(code);
        }
        Some("history") => history(&args[1..], &data),
        Some("show") => show(args.get(1).context(USAGE)?, &data),
        Some("export") => export(&args[1..], &data),
        Some("prune") => {
            let mut retention = Retention::from_env();
            if let Some(n) = flag_value(&args[1..], "--keep") {
                retention.keep_runs = n.parse()?;
            }
            if let Some(mb) = flag_value(&args[1..], "--max-frames-mb") {
                retention.max_frame_bytes = mb.parse::<u64>()? * 1024 * 1024;
            }
            let r = Store::open(&data)?.prune(&retention)?;
            println!(
                "deleted {} runs, {} recordings; frames now {:.1} MB",
                r.runs_deleted,
                r.recordings_deleted,
                r.frame_bytes as f64 / 1048576.0
            );
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

#[tokio::main]
async fn run(args: &[String], data: &std::path::Path) -> Result<i32> {
    let (ours, runner_args) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, vec![]),
    };
    let kind = match ours.first().map(String::as_str) {
        Some("playwright") => RunnerKind::Playwright,
        Some("vitest") => RunnerKind::Vitest,
        Some(s) if s.starts_with("cmd:") => RunnerKind::Command { program: s[4..].to_owned() },
        _ => bail!(USAGE),
    };
    let cwd = PathBuf::from(ours.get(1).context(USAGE)?);
    let record = !ours.iter().any(|a| a == "--no-record");
    let quiet = ours.iter().any(|a| a == "--quiet");

    let hub = Hub::default();
    let mut rx = hub.subscribe();
    let recorder = if record { Some(Recorder::spawn(data, &hub, Retention::from_env())?) } else { None };
    let ingest = IngestServer::bind(hub.clone())?;
    let orch = Orchestrator::new(
        hub.clone(),
        ingest.socket_path().to_owned(),
        adapters_dir(),
        tokio::runtime::Handle::current(),
    );
    let label = format!(
        "{} · {}",
        kind.name(),
        cwd.canonicalize()?.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    );
    let run = orch.start(RunSpec {
        label,
        kind,
        cwd,
        args: runner_args,
    })?;

    let mut frames: HashMap<String, u64> = HashMap::new();
    let mut stop = stop_signals()?;
    let mut cancelled = false;
    let exit_code = loop {
        tokio::select! {
            Some(sig) = stop.recv() => {
                eprintln!("{sig}: stopping run (SIGINT, escalating after {}s)…", testudo_core::CANCEL_GRACE.as_secs());
                cancelled = true;
                run.cancel();
            }
            msg = rx.recv() => {
                let Some(msg) = msg else { break 1 };
                match msg.message {
                    Message::Frame { header, .. } => *frames.entry(header.page_id).or_default() += 1,
                    Message::Event(Event::Stdio { line, .. }) => println!("  | {line}"),
                    Message::Event(Event::ProcessExited { code }) => {
                        break if cancelled { 130 } else { code.unwrap_or(1) };
                    }
                    Message::Event(ev) if !quiet => println!("{}", serde_json::to_string(&ev)?),
                    Message::Event(_) => {}
                }
            }
        }
    };

    hub.close();
    if let Some(r) = recorder {
        r.join();
    }
    for (page, n) in &frames {
        eprintln!("page {page}: {n} frames");
    }
    if record {
        eprintln!("recorded run {} in {}", run.id, data.display());
    }
    Ok(exit_code)
}

/// SIGINT, SIGTERM and SIGHUP all mean "stop the run cleanly".
fn stop_signals() -> Result<tokio::sync::mpsc::UnboundedReceiver<&'static str>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        for (kind, name) in [
            (SignalKind::interrupt(), "SIGINT"),
            (SignalKind::terminate(), "SIGTERM"),
            (SignalKind::hangup(), "SIGHUP"),
        ] {
            let mut s = signal(kind)?;
            let tx = tx.clone();
            tokio::spawn(async move {
                while s.recv().await.is_some() {
                    let _ = tx.send(name);
                }
            });
        }
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            let _ = tx.send("Ctrl+C");
        }
    });
    Ok(rx)
}

fn history(args: &[String], data: &std::path::Path) -> Result<()> {
    let limit = flag_value(args, "--limit").map(str::parse).transpose()?.unwrap_or(20);
    let store = Store::open(data)?;
    println!("{:<18} {:<11} {:>5} {:>5} {:>5} {:>8}  label", "run", "status", "pass", "fail", "skip", "secs");
    for r in store.list_runs(limit)? {
        let secs = r.ended_at.map(|e| format!("{:.1}", (e - r.started_at) / 1000.0)).unwrap_or_default();
        println!(
            "{:<18} {:<11} {:>5} {:>5} {:>5} {:>8}  {}",
            r.id, r.status, r.passed, r.failed, r.skipped, secs, r.label
        );
    }
    Ok(())
}

fn show(run_id: &str, data: &std::path::Path) -> Result<()> {
    let store = Store::open(data)?;
    let run = store.run(run_id)?.with_context(|| format!("no run {run_id}"))?;
    println!("{} [{}] {}  cwd={}", run.id, run.status, run.label, run.cwd);

    let events = store.run_events(run_id)?;
    println!("\ntests:");
    let mut results: Vec<(String, String)> = Vec::new();
    for (_, ev) in &events {
        match ev {
            Event::TestBegin { test_id, title, .. } => {
                results.retain(|(id, _)| id != test_id);
                results.push((test_id.clone(), format!("…  {title}")));
            }
            Event::TestEnd { test_id, status, duration_ms, .. } => {
                if let Some(r) = results.iter_mut().find(|(id, _)| id == test_id) {
                    let title = r.1[3..].to_owned();
                    r.1 = format!("{status:?} {title} ({:.0}ms)", duration_ms.unwrap_or(0.0));
                }
            }
            _ => {}
        }
    }
    for (_, line) in results {
        println!("  {line}");
    }

    println!("\npages:");
    for p in store.run_pages(run_id)? {
        let file = store.frame_path(run_id, &p.page_id);
        println!(
            "  {:<12} {:<9} frames={:<5} test={}  {}",
            p.page_id,
            p.browser.unwrap_or_default(),
            p.frames,
            p.test_id.unwrap_or_default(),
            if file.exists() { file.display().to_string() } else { "(no recording)".into() }
        );
    }
    println!("\n{} events", events.len());
    Ok(())
}

fn export(args: &[String], data: &std::path::Path) -> Result<()> {
    let [run_id, page_id, out, ..] = args else { bail!(USAGE) };
    let fps = flag_value(args, "--fps").map(str::parse).transpose()?.unwrap_or(25);
    let store = Store::open(data)?;
    export_mp4(
        &store.frame_path(run_id, page_id),
        std::path::Path::new(out),
        &ExportOptions { fps, ..Default::default() },
    )?;
    println!("wrote {out}");
    Ok(())
}
