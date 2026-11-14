//! Run one suite with Testudo instrumentation and print the event stream.
//! Useful for adapter development and as the seed of a CI agent.
//!
//! Usage: testudo-headless <playwright|vitest|cmd:PROGRAM> <project-dir> [-- runner args...]

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use testudo_core::{Hub, IngestServer, Orchestrator, RunSpec, RunnerKind, adapters_dir};
use testudo_protocol::{Event, Message};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let mut argv = std::env::args().skip(1);
    let kind = match argv.next().as_deref() {
        Some("playwright") => RunnerKind::Playwright,
        Some("vitest") => RunnerKind::Vitest,
        Some(s) if s.starts_with("cmd:") => RunnerKind::Command { program: s[4..].to_owned() },
        _ => bail!("usage: testudo-headless <playwright|vitest|cmd:PROGRAM> <project-dir> [-- args...]"),
    };
    let cwd = PathBuf::from(argv.next().context("missing project dir")?);
    let args: Vec<String> = argv.skip_while(|a| a == "--").collect();

    let hub = Hub::default();
    let mut rx = hub.subscribe();
    let ingest = IngestServer::bind(hub.clone())?;
    let orch = Orchestrator::new(
        hub,
        ingest.socket_path().to_owned(),
        adapters_dir(),
        tokio::runtime::Handle::current(),
    );
    let mut run = orch.start(RunSpec {
        label: "headless".into(),
        kind,
        cwd,
        args,
    })?;

    let mut frames: HashMap<String, (u64, usize)> = HashMap::new();
    let exit_code = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => run.cancel(),
            msg = rx.recv() => {
                let Some(msg) = msg else { break 1 };
                match msg.message {
                    Message::Frame { header, jpeg } => {
                        let e = frames.entry(header.page_id).or_default();
                        e.0 += 1;
                        e.1 += jpeg.len();
                    }
                    Message::Event(Event::Stdio { line, .. }) => println!("  | {line}"),
                    Message::Event(Event::ProcessExited { code }) => {
                        println!("[{}] process exited: {code:?}", msg.run_id);
                        break code.unwrap_or(1);
                    }
                    Message::Event(ev) => println!("[{}] {}", msg.run_id, serde_json::to_string(&ev)?),
                }
            }
        }
    };

    for (page, (n, bytes)) in &frames {
        println!("frames page={page} count={n} bytes={bytes}");
    }
    std::process::exit(exit_code);
}
