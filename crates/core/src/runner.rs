//! Spawning test runners with Testudo instrumentation injected from outside, so the
//! user's tests and fixtures stay untouched:
//!
//! - `NODE_OPTIONS=--import <adapters>/preload.mjs` instruments playwright-core in
//!   every Node process the runner starts (Playwright workers, Vitest's browser
//!   provider) and streams frames/network/console.
//! - Playwright: `PW_TEST_REPORTER=<adapters>/playwright-reporter.mjs`, which
//!   Playwright appends after the configured reporters. Nothing is written into the
//!   user's project. (Undocumented but long-standing; the VS Code extension relies
//!   on it.)
//! - Vitest: `--reporter` flags on the CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use testudo_protocol::{Event, Message, StdStream, env};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, watch};
use tracing::{Instrument, info_span, warn};

use crate::hub::{Hub, RunId};
use crate::procs::{self, Sig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerKind {
    Playwright,
    Vitest,
    /// Arbitrary command; still gets the preload and socket env, so any Node process
    /// using playwright-core is instrumented.
    Command {
        program: String,
    },
}

impl RunnerKind {
    pub fn name(&self) -> &str {
        match self {
            Self::Playwright => "playwright",
            Self::Vitest => "vitest",
            Self::Command { .. } => "command",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub label: String,
    pub kind: RunnerKind,
    pub cwd: PathBuf,
    /// Explicit config file (`-c` for Playwright, `--config` for Vitest).
    pub config: Option<PathBuf>,
    pub args: Vec<String>,
    /// The part of `args` the user gave (after the project config's).
    pub user_args: Vec<String>,
    /// Extra environment for the runner (project config rules). Testudo's own
    /// variables still win; `NODE_OPTIONS` and `PATH` are combined with them.
    pub env: Vec<(String, String)>,
}

impl RunSpec {
    /// The spec for running a discovered suite: the project config's args
    /// come before `args` (the user's runner args), and its env is added.
    /// The config is read now, so edits since the scan apply; a broken config
    /// or missing `env_file` is an error rather than a run without it.
    pub fn for_suite(suite: &crate::discover::Suite, args: Vec<String>) -> anyhow::Result<Self> {
        let kind = suite
            .kind
            .runner()
            .with_context(|| format!("{} suites can't run yet", suite.kind.label()))?;
        let rc = suite.run_config()?;
        Ok(Self {
            label: suite.name.clone(),
            kind,
            cwd: suite.dir.clone(),
            config: suite.config.clone(),
            args: rc.args.into_iter().chain(args.iter().cloned()).collect(),
            user_args: args,
            env: rc.env,
        })
    }
}

/// How long a cancelled run gets to shut down after SIGINT before escalating to
/// SIGTERM (then SIGKILL 2s later).
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);

pub struct RunHandle {
    pub id: RunId,
    cancel: Arc<watch::Sender<bool>>,
}

impl RunHandle {
    /// Ask the run to stop: SIGINT, then SIGTERM after `CANCEL_GRACE`, then SIGKILL;
    /// detached helpers are cleaned up after the runner exits.
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<Inner>,
}

struct Inner {
    hub: Hub,
    socket: PathBuf,
    adapters: PathBuf,
    rt: tokio::runtime::Handle,
    connections: crate::ingest::Connections,
    next_id: AtomicU64,
    /// Recording resolution for runs started from now on (settings).
    frame_max: std::sync::atomic::AtomicU32,
    frame_rate: std::sync::atomic::AtomicU32,
    /// Runs whose process has not exited yet, with their cancel switch.
    active: Mutex<HashMap<RunId, Arc<watch::Sender<bool>>>>,
    run_exited: Notify,
}

impl Orchestrator {
    pub fn new(ingest: &crate::IngestServer, hub: Hub, adapters: PathBuf, rt: tokio::runtime::Handle) -> Self {
        Self {
            inner: Arc::new(Inner {
                hub,
                socket: ingest.socket_path().to_owned(),
                adapters,
                rt,
                connections: ingest.connections(),
                next_id: AtomicU64::new(1),
                frame_max: std::sync::atomic::AtomicU32::new(crate::settings::DEFAULT_FRAME_MAX),
                frame_rate: std::sync::atomic::AtomicU32::new(crate::settings::DEFAULT_FRAME_RATE),
                active: Mutex::new(HashMap::new()),
                run_exited: Notify::new(),
            }),
        }
    }

    /// Recording resolution and frame rate for runs started after this call.
    pub fn set_recording(&self, settings: &crate::settings::Settings) {
        self.inner.frame_max.store(settings.frame_max(), Ordering::Relaxed);
        self.inner.frame_rate.store(settings.frame_rate(), Ordering::Relaxed);
    }

    pub fn active_runs(&self) -> usize {
        self.inner.active.lock().unwrap().len()
    }

    pub fn cancel_all(&self) {
        for tx in self.inner.active.lock().unwrap().values() {
            let _ = tx.send(true);
        }
    }

    /// Cancel every active run and wait for their processes to exit (see
    /// `RunHandle::cancel` for the escalation). Each run's
    /// `ProcessExited` is published before this returns, so a recorder can then
    /// be drained with a complete history.
    pub async fn shutdown(&self) {
        self.cancel_all();
        let deadline = tokio::time::Instant::now() + CANCEL_GRACE + Duration::from_secs(6);
        loop {
            let notified = self.inner.run_exited.notified();
            if self.active_runs() == 0 {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                warn!(remaining = self.active_runs(), "runs did not exit in time");
                return;
            }
        }
    }

    /// `shutdown` for callers outside the runtime (the GUI's main thread).
    pub fn shutdown_blocking(&self) {
        let this = self.clone();
        self.inner.rt.block_on(async move { this.shutdown().await });
    }

    /// Spawn a run. Safe to call from any thread.
    pub fn start(&self, spec: RunSpec) -> Result<RunHandle> {
        let inner = &self.inner;
        // Unique across restarts: run ids key the persistent history.
        let id = format!(
            "{}-{}",
            crate::hub::now_ms() as u64,
            inner.next_id.fetch_add(1, Ordering::Relaxed)
        );
        // One span per run, from spawn to exit (Testudo's own tracing).
        let span = info_span!(
            "run",
            run.id = %id,
            run.label = %spec.label,
            runner = spec.kind.name(),
            cwd = %spec.cwd.display(),
            exit_code = tracing::field::Empty,
            cancelled = tracing::field::Empty,
        );
        let _entered = span.enter();
        let cwd = spec
            .cwd
            .canonicalize()
            .with_context(|| format!("project dir {}", spec.cwd.display()))?;

        let Launch {
            program,
            args,
            envs,
            note,
        } = build_command(&spec.kind, &cwd, spec.config.as_deref(), &spec.args, &inner.adapters)?;

        // Project config env replaces inherited values, except NODE_OPTIONS
        // (the preload is still added) and PATH (the project's Node still
        // goes first).
        let spec_env = |name: &str| spec.env.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let preload = inner.adapters.join("preload.mjs");
        let mut node_options = format!("--import {}", file_url(&preload));
        if let Some(existing) = spec_env("NODE_OPTIONS").or_else(|| std::env::var("NODE_OPTIONS").ok()) {
            node_options = format!("{node_options} {existing}");
        }

        // Honour the project's .nvmrc/.node-version. Reported in the run output so
        // a version mismatch is visible rather than silent.
        let mut notes: Vec<String> = note.into_iter().collect();
        if !spec.env.is_empty() {
            // Names only: values may be secrets.
            let names: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
            notes.push(format!(
                "[testudo] env from {}: {}",
                crate::project_config::CONFIG_PATH,
                names.join(", ")
            ));
        }
        let mut path_env = spec_env("PATH")
            .map(Into::into)
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        if !matches!(spec.kind, RunnerKind::Command { .. }) {
            match crate::node_env::resolve(&cwd) {
                Some(Ok(choice)) => {
                    let mut paths = vec![choice.bin_dir.clone()];
                    paths.extend(std::env::split_paths(&path_env));
                    path_env = std::env::join_paths(paths).unwrap_or(path_env);
                    notes.push(format!(
                        "[testudo] node {} ({} wants {}{})",
                        choice.version,
                        crate::tilde(&choice.source),
                        choice.wanted,
                        if choice.exact { "" } else { "; closest installed" }
                    ));
                }
                Some(Err(reason)) => notes.push(format!("[testudo] {reason}")),
                None => {}
            }
        }

        let _guard = inner.rt.enter();
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&cwd)
            .envs(
                spec.env
                    .iter()
                    .filter(|(k, _)| k != "NODE_OPTIONS" && k != "PATH")
                    .map(|(k, v)| (k, v)),
            )
            .env("NODE_OPTIONS", node_options)
            .env(env::SOCKET, &inner.socket)
            .env(env::RUN_ID, &id)
            .env(env::PROJECT_ROOT, &cwd)
            .env(env::FRAME_MAX, inner.frame_max.load(Ordering::Relaxed).to_string())
            .env(env::FRAME_RATE, inner.frame_rate.load(Ordering::Relaxed).to_string())
            .env("FORCE_COLOR", "0")
            .env("PATH", &path_env)
            // Testudo's own telemetry settings are not for the test run.
            .env_remove("TESTUDO_OTLP_ENDPOINT")
            .env_remove("TESTUDO_OTLP_TOKEN")
            .env_remove("TESTUDO_OTLP_LOGS")
            .env_remove("TESTUDO_OTLP_FILTER")
            .env_remove("TESTUDO_LOG_FILE")
            .envs(envs)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {program} {}", args.join(" ")))?;

        let hub = inner.hub.clone();
        hub.publish(
            &id,
            Message::Event(Event::RunStarted {
                label: spec.label.clone(),
                runner: spec.kind.name().to_owned(),
                cwd: cwd.display().to_string(),
                args: spec.args.clone(),
                user_args: Some(spec.user_args.clone()),
            }),
        );
        for line in notes {
            hub.publish(
                &id,
                Message::Event(Event::Stdio {
                    stream: StdStream::Stderr,
                    line,
                }),
            );
        }
        pipe_lines(child.stdout.take().unwrap(), StdStream::Stdout, hub.clone(), id.clone());
        pipe_lines(child.stderr.take().unwrap(), StdStream::Stderr, hub.clone(), id.clone());

        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let cancel_tx = Arc::new(cancel_tx);
        inner.active.lock().unwrap().insert(id.clone(), cancel_tx.clone());
        let run_id = id.clone();
        let orch = self.clone();
        let run_span = span.clone();
        tokio::spawn(
            async move {
                let pgid = child.id().map(|p| p as i32);
                let mut snapshot = Vec::new();
                let status = tokio::select! {
                    s = child.wait() => s,
                    // Drop the watch::Ref (not Send) before awaiting anything else.
                    _ = async { cancel_rx.wait_for(|c| *c).await.is_ok() } => {
                        run_span.record("cancelled", true);
                        if let Some(pgid) = pgid {
                            // Before signalling: the tree is intact, so detached helpers
                            // (Playwright's webServer) are still findable as descendants.
                            snapshot = procs::descendants(pgid);
                        }
                        stop(&mut child, pgid).instrument(info_span!("stop_process_group")).await
                    }
                };
                if let Some(pgid) = pgid {
                    // Leftovers in the runner's own group (normal exit or not), then
                    // anything from the snapshot that escaped into other groups.
                    procs::signal_group(pgid, Sig::Term);
                    if procs::signal_survivors(&snapshot, pgid, Sig::Term) > 0 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        procs::signal_survivors(&snapshot, pgid, Sig::Kill);
                    }
                }
                // Announce the exit only after everything the run's processes sent
                // has been published, so consumers see its final results first.
                let drained = orch
                    .inner
                    .connections
                    .drained(&run_id, Duration::from_secs(3))
                    .instrument(info_span!("drain_adapter_connections"))
                    .await;
                if !drained {
                    warn!(run = %run_id, "adapter connections still open after exit");
                }
                let code = status.ok().and_then(|s| s.code());
                if let Some(c) = code {
                    run_span.record("exit_code", c);
                }
                hub.publish(&run_id, Message::Event(Event::ProcessExited { code }));
                orch.inner.active.lock().unwrap().remove(&run_id);
                orch.inner.run_exited.notify_waiters();
            }
            .instrument(span.clone()),
        );

        Ok(RunHandle { id, cancel: cancel_tx })
    }
}

struct Launch {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
    note: Option<String>,
}

fn build_command(
    kind: &RunnerKind,
    cwd: &Path,
    config: Option<&Path>,
    user_args: &[String],
    adapters: &Path,
) -> Result<Launch> {
    let user_config = user_args.iter().any(|a| a == "-c" || a.starts_with("--config"));
    let config = config.filter(|_| !user_config);
    match kind {
        RunnerKind::Playwright => {
            let (program, note) = local_bin(cwd, "playwright", &["@playwright/test", "playwright"])?;
            let mut args = vec!["test".to_owned()];
            if let Some(c) = config {
                args.extend(["-c".into(), c.display().to_string()]);
            }
            args.extend(user_args.iter().cloned());
            let reporter = adapters.join("playwright-reporter.mjs").display().to_string();
            Ok(Launch {
                program,
                args,
                envs: vec![("PW_TEST_REPORTER".into(), reporter)],
                note,
            })
        }
        RunnerKind::Vitest => {
            let (program, note) = local_bin(cwd, "vitest", &["vitest"])?;
            let mut args = Vec::new();
            let subcommands = ["run", "watch", "dev", "related", "bench"];
            if !user_args.first().is_some_and(|a| subcommands.contains(&a.as_str())) {
                args.push("run".into());
            }
            if let Some(c) = config {
                args.push(format!("--config={}", c.display()));
            }
            args.push("--reporter=default".into());
            args.push(format!("--reporter={}", adapters.join("vitest-reporter.mjs").display()));
            // Test line numbers: shown on tiles and needed to re-run one test.
            if !user_args.iter().any(|a| a.starts_with("--includeTaskLocation")) {
                args.push("--includeTaskLocation".into());
            }
            args.extend(user_args.iter().cloned());
            Ok(Launch {
                program,
                args,
                envs: vec![],
                note,
            })
        }
        RunnerKind::Command { program } => Ok(Launch {
            program: program.clone(),
            args: user_args.to_vec(),
            envs: vec![],
            note: None,
        }),
    }
}

/// The project's own runner binary (nearest `node_modules/.bin` upward), with a
/// note naming the version. Never falls back to `npx`, which would silently
/// download whatever version is latest.
fn local_bin(cwd: &Path, name: &str, pkgs: &[&str]) -> Result<(String, Option<String>)> {
    let Some(bin) = crate::discover::find_bin(cwd, name) else {
        anyhow::bail!(
            "{name} is not installed in {} (no node_modules/.bin/{name} there or above); \
             run your package manager's install, or point Testudo at the directory that has it",
            cwd.display()
        );
    };
    let version = pkgs
        .iter()
        .find_map(|p| crate::discover::package_version(cwd, p))
        .unwrap_or_else(|| "?".into());
    let note = format!("[testudo] {name} {version} ({})", crate::tilde(&bin));
    Ok((bin.display().to_string(), Some(note)))
}

fn file_url(path: &Path) -> String {
    let s = path.display().to_string();
    let mut out = String::from("file://");
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn pipe_lines<R: AsyncRead + Unpin + Send + 'static>(reader: R, stream: StdStream, hub: Hub, run_id: RunId) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = strip_ansi(&line);
            hub.publish(&run_id, Message::Event(Event::Stdio { stream, line }));
        }
    });
}

/// Drop ANSI escape sequences (CSI `ESC [ ... final` and two-byte `ESC x`). Some
/// runners colour output regardless of FORCE_COLOR.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Stop like a terminal would: SIGINT first so runners run their own teardown
/// (Playwright stops its webServer, reporters flush), then SIGTERM, then SIGKILL.
async fn stop(child: &mut tokio::process::Child, pgid: Option<i32>) -> std::io::Result<std::process::ExitStatus> {
    let Some(pgid) = pgid else { return child.wait().await };
    for (sig, wait) in [(Sig::Int, CANCEL_GRACE), (Sig::Term, Duration::from_secs(2))] {
        procs::signal_group(pgid, sig);
        if let Ok(status) = tokio::time::timeout(wait, child.wait()).await {
            return status;
        }
    }
    procs::signal_group(pgid, Sig::Kill);
    child.wait().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[32m- 3\x1b[39m ok"), "- 3 ok");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn file_url_escapes() {
        assert_eq!(file_url(Path::new("/a b/c.mjs")), "file:///a%20b/c.mjs");
    }
}
