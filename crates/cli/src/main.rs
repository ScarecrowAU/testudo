//! Headless Testudo. Seed of the CI agent.
//!
//! testudo-cli suites <dir>
//! testudo-cli run [FILTER...] <dir> [--no-record] [--quiet] [-- runner args...]
//! testudo-cli run cmd:PROGRAM <dir> [-- args...]
//! testudo-cli history [--limit N]
//! testudo-cli show <run-id>
//! testudo-cli export <run-id> <page-id> <out.mp4> [--fps N]
//! testudo-cli prune [--keep N] [--max-frames-mb N]
//! testudo-cli update [--check] [--channel stable|bleeding-edge]

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use testudo_core::discover::{Suite, discover};
use testudo_core::{Hub, IngestServer, Orchestrator, RunSpec, RunnerKind, adapters_dir};
use testudo_protocol::{Event, Message};
use testudo_store::export::{ExportOptions, export_mp4};
use testudo_store::{Recorder, Retention, Store, default_data_dir};

const USAGE: &str = "usage:
  testudo-cli suites <dir>                 list the test suites found under <dir>
  testudo-cli run [FILTER...] <dir> [--no-record] [--quiet] [-- runner args...]
                                           run the discovered suites, or only those matching a
                                           FILTER: playwright, vitest or part of a suite name
  testudo-cli run cmd:PROGRAM <dir> [-- args...]
  testudo-cli history [--limit N]
  testudo-cli show <run-id>
  testudo-cli export <run-id> <page-id> <out.mp4> [--fps N]
  testudo-cli prune [--keep N] [--max-frames-mb N]
  testudo-cli update [--check] [--channel stable|bleeding-edge]
                                           install a newer Testudo. --check only reports it.
                                           The channel defaults to the one in Settings.

data dir: $TESTUDO_DATA_DIR, else $XDG_DATA_HOME/testudo, else ~/.local/share/testudo
retention: TESTUDO_KEEP_RUNS (200), TESTUDO_MAX_FRAMES_MB (5120), TESTUDO_KEEP_FRAMES=all|failed";

fn main() -> Result<()> {
    exit_quietly_on_closed_stdout();
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
        Some("suites") => {
            let dir = PathBuf::from(args.get(1).context(USAGE)?);
            print_suites(&discover(&dir));
            Ok(())
        }
        Some("history") => history(&args[1..], &data),
        Some("show") => show(args.get(1).context(USAGE)?, &data),
        Some("export") => export(&args[1..], &data),
        Some("update") => update(&args[1..]),
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
                "deleted {} runs and {} recordings; frames now take {:.1} MB",
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

fn update(args: &[String]) -> Result<()> {
    use testudo_core::settings::{Settings, UpdateChannel};
    let channel = match flag_value(args, "--channel") {
        Some(name) => UpdateChannel::parse(name).with_context(|| format!("unknown channel {name}\n{USAGE}"))?,
        None => Settings::load().update_channel,
    };
    println!(
        "Testudo {} on the {} channel",
        testudo_update::current(),
        channel.name()
    );
    let Some(update) = testudo_update::check(channel)? else {
        println!("Up to date.");
        return Ok(());
    };
    println!("{} is available: {}", update.label, update.url);
    if args.iter().any(|a| a == "--check") {
        return Ok(());
    }
    let install = testudo_update::Install::detect()?;
    testudo_update::install(&update, &install, &mut |line| eprintln!("{line}"))?;
    println!("Updated to {}. Restart Testudo to use it.", update.label);
    Ok(())
}

/// When stdout is a pipe the reader closed early (`testudo-cli history |
/// head`), std's print macros panic. Exit like a tool killed by SIGPIPE
/// instead. (SIGPIPE itself stays ignored: it would also kill us on a write
/// to a runner socket that closed.)
fn exit_quietly_on_closed_stdout() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        if msg.starts_with("failed printing to stdout") && msg.contains("Broken pipe") {
            std::process::exit(141);
        }
        default(info)
    }));
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

#[tokio::main]
async fn run(args: &[String], data: &std::path::Path) -> Result<i32> {
    let (ours, runner_args) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, vec![]),
    };
    let record = !ours.iter().any(|a| a == "--no-record");
    let quiet = ours.iter().any(|a| a == "--quiet");
    let positional: Vec<&String> = ours.iter().filter(|a| !a.starts_with("--")).collect();
    let Some((dir, filters)) = positional.split_last() else {
        bail!(USAGE)
    };
    let dir = PathBuf::from(dir.as_str());
    if !dir.is_dir() {
        bail!("{} is not a directory\n\n{USAGE}", dir.display());
    }

    let specs: Vec<RunSpec> = match filters.first().and_then(|f| f.strip_prefix("cmd:")) {
        Some(program) => vec![RunSpec {
            label: format!("command · {program}"),
            kind: RunnerKind::Command {
                program: program.to_owned(),
            },
            cwd: dir.clone(),
            config: None,
            args: runner_args.clone(),
            user_args: runner_args,
            env: vec![],
        }],
        None => {
            let suites = discover(&dir);
            let chosen: Vec<&Suite> = suites
                .iter()
                .filter(|s| filters.is_empty() || filters.iter().any(|f| s.matches(f)))
                .collect();
            for s in chosen.iter().filter(|s| !s.runnable()) {
                eprintln!("skipping {}: {}", s.name, s.problems.join("; "));
            }
            let specs: Vec<RunSpec> = chosen
                .iter()
                .filter(|s| s.runnable())
                .filter_map(|s| match RunSpec::for_suite(s, runner_args.clone()) {
                    Ok(spec) => Some(spec),
                    Err(e) => {
                        eprintln!("skipping {}: {e:#}", s.name);
                        None
                    }
                })
                .collect();
            if specs.is_empty() {
                eprintln!("no runnable suites matched under {}", dir.display());
                print_suites(&suites);
                return Ok(2);
            }
            specs
        }
    };

    let hub = Hub::default();
    let mut rx = hub.subscribe();
    let recorder = if record {
        Some(Recorder::spawn(data, &hub, Retention::from_env())?)
    } else {
        None
    };
    let ingest = IngestServer::bind(hub.clone())?;
    let orch = Orchestrator::new(&ingest, hub.clone(), adapters_dir(), tokio::runtime::Handle::current());
    orch.set_recording(&testudo_core::settings::Settings::load());

    let mut labels: HashMap<String, String> = HashMap::new();
    for spec in specs {
        let label = spec.label.clone();
        match orch.start(spec) {
            Ok(run) => {
                eprintln!("started {label} ({})", run.id);
                labels.insert(run.id.clone(), label);
            }
            Err(e) => eprintln!("couldn't start {label}: {e:#}"),
        }
    }
    let many = labels.len() > 1;

    let mut results: Vec<(String, Option<i32>)> = Vec::new();
    let mut stop = stop_signals()?;
    let mut cancelled = false;
    while results.len() < labels.len() {
        tokio::select! {
            Some(sig) = stop.recv() => {
                eprintln!("{sig}: stopping runs with SIGINT, then forcing them after {}s…", testudo_core::CANCEL_GRACE.as_secs());
                cancelled = true;
                orch.cancel_all();
            }
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                let tag = if many { format!("[{}] ", labels.get(&msg.run_id).map_or("?", |s| s)) } else { String::new() };
                match msg.message {
                    Message::Frame { .. } => {}
                    Message::Event(Event::Stdio { line, .. }) => println!("{tag}| {line}"),
                    Message::Event(Event::ProcessExited { code }) => {
                        results.push((msg.run_id.clone(), code));
                    }
                    Message::Event(ev) if !quiet => println!("{tag}{}", serde_json::to_string(&ev)?),
                    Message::Event(_) => {}
                }
            }
        }
    }

    hub.close();
    if let Some(r) = recorder {
        r.join();
    }
    for (id, code) in &results {
        let status = match code {
            Some(0) => "passed".to_owned(),
            Some(c) => format!("failed (exit {c})"),
            None => "stopped".to_owned(),
        };
        eprintln!("{:<8} {}  {id}", status, labels.get(id).map_or("?", |s| s));
    }
    if record {
        eprintln!("recorded in {}", data.display());
    }
    let worst = results
        .iter()
        .map(|(_, c)| c.unwrap_or(1))
        .find(|c| *c != 0)
        .unwrap_or(0);
    Ok(if cancelled { 130 } else { worst })
}

fn print_suites(suites: &[Suite]) {
    if suites.is_empty() {
        println!("no test suites found");
        return;
    }
    for s in suites {
        let version = s
            .tool
            .as_ref()
            .and_then(|t| t.version.as_deref())
            .map(|v| format!(" {v}"))
            .unwrap_or_default();
        let state = if s.runnable() { "ready" } else { "-" };
        println!("{state:<6} {}{version}    [{}]", s.name, s.id);
        for p in &s.problems {
            println!("         ✘ {p}");
        }
        for w in &s.warnings {
            println!("         ⚠ {w}");
        }
    }
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
    println!(
        "{:<18} {:<11} {:>5} {:>5} {:>5} {:>8}  label",
        "run", "status", "pass", "fail", "skip", "secs"
    );
    for r in store.list_runs(limit)? {
        let secs = r
            .ended_at
            .map(|e| format!("{:.1}", (e - r.started_at) / 1000.0))
            .unwrap_or_default();
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
            Event::TestEnd {
                test_id,
                status,
                duration_ms,
                ..
            } => {
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
            if file.exists() {
                file.display().to_string()
            } else {
                "(no recording)".into()
            }
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
        &ExportOptions {
            fps,
            ..Default::default()
        },
    )?;
    println!("wrote {out}");
    Ok(())
}
