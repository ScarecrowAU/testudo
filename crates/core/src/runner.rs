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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use testudo_protocol::{Event, Message, StdStream, env};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, watch};
use tracing::warn;

use crate::hub::{Hub, RunId};
use crate::procs::{self, Sig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerKind {
    Playwright,
    Vitest,
    /// Arbitrary command; still gets the preload and socket env, so any Node process
    /// using playwright-core is instrumented.
    Command { program: String },
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
    pub args: Vec<String>,
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
    next_id: AtomicU64,
    /// Runs whose process has not exited yet, with their cancel switch.
    active: Mutex<HashMap<RunId, Arc<watch::Sender<bool>>>>,
    run_exited: Notify,
}

impl Orchestrator {
    pub fn new(hub: Hub, socket: PathBuf, adapters: PathBuf, rt: tokio::runtime::Handle) -> Self {
        Self {
            inner: Arc::new(Inner {
                hub,
                socket,
                adapters,
                rt,
                next_id: AtomicU64::new(1),
                active: Mutex::new(HashMap::new()),
                run_exited: Notify::new(),
            }),
        }
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
        let cwd = spec
            .cwd
            .canonicalize()
            .with_context(|| format!("project dir {}", spec.cwd.display()))?;

        let Launch { program, args, envs } = build_command(&spec.kind, &cwd, &spec.args, &inner.adapters);

        let preload = inner.adapters.join("preload.mjs");
        let mut node_options = format!("--import {}", file_url(&preload));
        if let Ok(existing) = std::env::var("NODE_OPTIONS") {
            node_options = format!("{node_options} {existing}");
        }

        // Honour the project's .nvmrc/.node-version. Reported in the run output so
        // a version mismatch is visible rather than silent.
        let mut notes = Vec::new();
        let mut path_env = std::env::var_os("PATH").unwrap_or_default();
        if !matches!(spec.kind, RunnerKind::Command { .. }) {
            match crate::node_env::resolve(&cwd) {
                Some(Ok(choice)) => {
                    let mut paths = vec![choice.bin_dir.clone()];
                    paths.extend(std::env::split_paths(&path_env));
                    path_env = std::env::join_paths(paths).unwrap_or(path_env);
                    notes.push(format!(
                        "[testudo] node {} ({} wants {}{})",
                        choice.version,
                        choice.source.display(),
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
            .env("NODE_OPTIONS", node_options)
            .env(env::SOCKET, &inner.socket)
            .env(env::RUN_ID, &id)
            .env(env::PROJECT_ROOT, &cwd)
            .env("FORCE_COLOR", "0")
            .env("PATH", &path_env)
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
            }),
        );
        for line in notes {
            hub.publish(&id, Message::Event(Event::Stdio { stream: StdStream::Stderr, line }));
        }
        pipe_lines(child.stdout.take().unwrap(), StdStream::Stdout, hub.clone(), id.clone());
        pipe_lines(child.stderr.take().unwrap(), StdStream::Stderr, hub.clone(), id.clone());

        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let cancel_tx = Arc::new(cancel_tx);
        inner.active.lock().unwrap().insert(id.clone(), cancel_tx.clone());
        let run_id = id.clone();
        let orch = self.clone();
        tokio::spawn(async move {
            let pgid = child.id().map(|p| p as i32);
            let mut snapshot = Vec::new();
            let status = tokio::select! {
                s = child.wait() => s,
                // Drop the watch::Ref (not Send) before awaiting anything else.
                _ = async { cancel_rx.wait_for(|c| *c).await.is_ok() } => {
                    if let Some(pgid) = pgid {
                        // Before signalling: the tree is intact, so detached helpers
                        // (Playwright's webServer) are still findable as descendants.
                        snapshot = procs::descendants(pgid);
                    }
                    stop(&mut child, pgid).await
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
            let code = status.ok().and_then(|s| s.code());
            hub.publish(&run_id, Message::Event(Event::ProcessExited { code }));
            orch.inner.active.lock().unwrap().remove(&run_id);
            orch.inner.run_exited.notify_waiters();
        });

        Ok(RunHandle { id, cancel: cancel_tx })
    }
}

struct Launch {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
}

fn build_command(kind: &RunnerKind, cwd: &Path, user_args: &[String], adapters: &Path) -> Launch {
    match kind {
        RunnerKind::Playwright => {
            let (program, mut args) = node_bin(cwd, "playwright");
            args.push("test".into());
            args.extend(user_args.iter().cloned());
            let reporter = adapters.join("playwright-reporter.mjs").display().to_string();
            Launch {
                program,
                args,
                envs: vec![("PW_TEST_REPORTER".into(), reporter)],
            }
        }
        RunnerKind::Vitest => {
            let (program, mut args) = node_bin(cwd, "vitest");
            let subcommands = ["run", "watch", "dev", "related", "bench"];
            if !user_args.first().is_some_and(|a| subcommands.contains(&a.as_str())) {
                args.push("run".into());
            }
            args.push("--reporter=default".into());
            args.push(format!("--reporter={}", adapters.join("vitest-reporter.mjs").display()));
            args.extend(user_args.iter().cloned());
            Launch { program, args, envs: vec![] }
        }
        RunnerKind::Command { program } => Launch {
            program: program.clone(),
            args: user_args.to_vec(),
            envs: vec![],
        },
    }
}

/// Prefer the project's local binary; fall back to `npx`.
fn node_bin(cwd: &Path, name: &str) -> (String, Vec<String>) {
    let local = cwd.join("node_modules/.bin").join(name);
    if local.exists() {
        (local.display().to_string(), vec![])
    } else {
        ("npx".into(), vec![name.into()])
    }
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
